//! Diffusion gradients that are **not** a sibling pair beside their image.
//!
//! Every case here returned zero rows before docs/adr/0003, and none was covered by a test —
//! which is why the defect stayed silent through the ADR 0006 foreign-key work that first
//! made it loud. The common cause was `process_diffusion_file` deriving the image by swapping
//! the extension on the gradient file's stem, so anything but `<stem>.nii.gz` sitting beside
//! `<stem>.bval` produced a path the dataset did not contain.
//!
//! `curated.rs::ds000117_diffusion_and_associations` is the sibling-pair counterpart, and its
//! numbers are deliberately unchanged.

mod common;

use bidslake::db::BidsDb;
use common::{bids_example, ingest};
use std::fs;

/// `(diffusion rows, distinct images, distinct .bval files)`.
fn shape(db: &BidsDb) -> anyhow::Result<(i64, i64, i64)> {
    Ok(db.conn.query_row(
        "SELECT (SELECT COUNT(*) FROM diffusion), \
                (SELECT COUNT(DISTINCT file_id) FROM diffusion), \
                (SELECT COUNT(DISTINCT file_id) FROM bvals)",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?)
}

/// A dataset-root `dwi.bval`/`dwi.bvec` applies to every image below it. ds114 has 10
/// subjects x 2 sessions, so one stored gradient set answers for 20 images.
///
/// This is the case docs/adr/0003 opens with, and it yielded **zero** rows before: the
/// synthesized `dwi.nii.gz` is nothing on disk, so the write was skipped entirely.
#[tokio::test]
async fn a_root_level_gradient_set_reaches_every_image() -> anyhow::Result<()> {
    let db = ingest(bids_example("ds114")).await?;

    let (rows, images, bval_files) = shape(&db)?;
    assert_eq!(bval_files, 1, "one stored copy, not one per image");
    assert_eq!(images, 20, "10 subjects x 2 sessions");
    assert_eq!(rows, 20 * 71, "71 volumes, fanned out to all 20 images");

    // Stored once: the fan-out is the view's, and this is what makes it cheap.
    let stored: i64 = db
        .conn
        .query_row("SELECT COUNT(*) FROM bvals", [], |r| r.get(0))?;
    assert_eq!(stored, 71);

    // Both halves of the pair resolve, so no volume is missing its direction.
    let missing_bvec: i64 = db.conn.query_row(
        "SELECT COUNT(*) FROM diffusion WHERE bvec_x IS NULL OR bvec_y IS NULL OR bvec_z IS NULL",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(missing_bvec, 0);

    // Every image gets the *same* values — one inherited set, not a per-image guess.
    let distinct_first_bvals: i64 = db.conn.query_row(
        "SELECT COUNT(DISTINCT bval) FROM diffusion WHERE volume_idx = 0",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(distinct_first_bvals, 1);

    // And the rows are addressable by BIDS concept, because the view keys on the image.
    let sessions: i64 = db.conn.query_row(
        "SELECT COUNT(DISTINCT ses) FROM diffusion JOIN all_files USING (file_id)",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(sessions, 2);

    // The provenance columns name the files the values came from.
    let (bval_src, bvec_src): (String, String) = db.conn.query_row(
        "SELECT bv.file_path, bc.file_path FROM diffusion d \
         JOIN file_registry bv ON bv.file_id = d.bval_file_id \
         JOIN file_registry bc ON bc.file_id = d.bvec_file_id LIMIT 1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    assert_eq!(
        (bval_src.as_str(), bvec_src.as_str()),
        ("dwi.bval", "dwi.bvec")
    );
    Ok(())
}

/// The same inherited shape without sessions, so the fix is not ds114-shaped.
#[tokio::test]
async fn a_root_level_gradient_set_works_without_sessions() -> anyhow::Result<()> {
    let db = ingest(bids_example("genetics_ukbb")).await?;
    let (rows, images, bval_files) = shape(&db)?;
    assert_eq!(bval_files, 1);
    assert_eq!(images, 14);
    assert_eq!(rows, 14 * 65);
    Ok(())
}

/// A sibling pair beside an **uncompressed** `.nii`. Nothing to do with inheritance — the old
/// code hardcoded `.nii.gz` when synthesizing the image path, so `dwi_deriv` (which BIDS
/// permits, and which the schema's own `\.nii(\.gz)?$` selector always matched) was skipped.
#[tokio::test]
async fn an_uncompressed_image_gets_its_gradients() -> anyhow::Result<()> {
    let db = ingest(bids_example("dwi_deriv")).await?;
    let (rows, images, _) = shape(&db)?;
    assert_eq!(images, 1);
    assert!(rows > 0, "the .nii image should have gradient rows");

    let extension: String = db.conn.query_row(
        "SELECT DISTINCT extension FROM diffusion JOIN all_files USING (file_id)",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(extension, ".nii", "not .nii.gz — that was the bug");
    Ok(())
}

/// A `.bval` at the dataset root and a `.bvec` beside the image: legal BIDS, and the case the
/// old writer could not represent at all. It paired the two through a synthesized image path,
/// so a split pair hashed to two different keys, neither had both halves, and the
/// both-present guard dropped **both**.
///
/// Now nothing pairs them at write time; the view does it through the image each is
/// associated with. No vendored dataset has this shape.
#[tokio::test]
async fn a_pair_split_across_inheritance_levels_still_zips() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path();
    fs::create_dir_all(root.join("sub-01/dwi"))?;
    fs::write(
        root.join("dataset_description.json"),
        r#"{"Name":"split gradients","BIDSVersion":"1.11.1"}"#,
    )?;
    fs::write(root.join("sub-01/dwi/sub-01_dwi.nii.gz"), b"")?;
    // Shared b-values at the root...
    fs::write(root.join("dwi.bval"), "0 1000 2000")?;
    // ...but this subject's own directions beside the image.
    fs::write(
        root.join("sub-01/dwi/sub-01_dwi.bvec"),
        "0 0.707 0.577\n0 0.707 0.577\n0 0 0.577",
    )?;

    let db = ingest(root).await?;

    let rows: Vec<(i64, f64, f64)> = {
        let mut stmt = db
            .conn
            .prepare("SELECT volume_idx, bval, bvec_z FROM diffusion ORDER BY volume_idx")?;
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<Result<_, _>>()?
    };
    assert_eq!(rows.len(), 3, "the split pair must still produce volumes");
    assert_eq!(
        rows.iter().map(|r| r.1).collect::<Vec<_>>(),
        vec![0.0, 1000.0, 2000.0]
    );
    assert_eq!(
        rows[2].2, 0.577,
        "the sibling .bvec supplies the directions"
    );

    // The two halves genuinely came from different files.
    let same_file: bool = db.conn.query_row(
        "SELECT bval_file_id = bvec_file_id FROM diffusion LIMIT 1",
        [],
        |r| r.get(0),
    )?;
    assert!(!same_file);
    Ok(())
}

/// `meta.associations.bval` selects `intersects([suffix], ['dwi', 'epi'])`, so a fieldmap
/// `epi` image with its own gradients resolves them too — the schema said so all along, but
/// the stem-swap only ever looked for a file named after the source, which happened to work
/// for `dwi` and was never exercised for `epi`. No vendored dataset ships an `*_epi.bval`.
#[tokio::test]
async fn an_epi_fieldmap_gets_its_gradients() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path();
    fs::create_dir_all(root.join("sub-01/fmap"))?;
    fs::write(
        root.join("dataset_description.json"),
        r#"{"Name":"epi gradients","BIDSVersion":"1.11.1"}"#,
    )?;
    fs::write(root.join("sub-01/fmap/sub-01_dir-AP_epi.nii.gz"), b"")?;
    fs::write(root.join("sub-01/fmap/sub-01_dir-AP_epi.bval"), "0 0")?;
    fs::write(
        root.join("sub-01/fmap/sub-01_dir-AP_epi.bvec"),
        "0 0\n0 0\n0 0",
    )?;

    let db = ingest(root).await?;
    let (suffix, rows): (String, i64) = db.conn.query_row(
        "SELECT f.suffix, COUNT(*) FROM diffusion d JOIN all_files f USING (file_id) GROUP BY 1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    assert_eq!(suffix, "epi");
    assert_eq!(rows, 2);
    Ok(())
}

/// A gradient file whose image the dataset does not ship. Its values are **kept** — they are
/// rows of a file that exists — but it contributes nothing to `diffusion`, which is keyed by
/// image and has none to key on.
///
/// Before, the values were parsed and then silently discarded, so a typo'd stem and a
/// deliberately image-less gradient file were indistinguishable from a file that never
/// existed. Now the registry and `bvals` both say what is there.
#[tokio::test]
async fn an_orphan_gradient_file_is_stored_but_describes_nothing() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path();
    fs::create_dir_all(root.join("sub-01/dwi"))?;
    fs::write(
        root.join("dataset_description.json"),
        r#"{"Name":"orphan gradients","BIDSVersion":"1.11.1"}"#,
    )?;
    // A `.bval` with no image anywhere in the dataset.
    fs::write(root.join("sub-01/dwi/sub-01_dwi.bval"), "0 1000")?;

    let db = ingest(root).await?;
    let (rows, images, bval_files) = shape(&db)?;
    assert_eq!(bval_files, 1, "the values are kept");
    assert_eq!(rows, 0, "but describe no image");
    assert_eq!(images, 0);

    let registered: i64 = db.conn.query_row(
        "SELECT COUNT(*) FROM file_registry WHERE file_path LIKE '%.bval'",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(registered, 1, "the orphan .bval has its own registry row");
    Ok(())
}

/// `asl_context` is the third per-volume instance of the same shape, and one nothing
/// consumed before: an `*_aslcontext.tsv` is one row per volume of its ASL series, the
/// schema has declared the `aslcontext` association (with `inherit: true`) all along, and
/// nothing linked the two — so "the volume types of this ASL image" needed an entity guess.
///
/// It shares no code with diffusion beyond the `describes` block, which is the point: the
/// mechanism generalizes rather than being fitted to gradients.
#[tokio::test]
async fn asl_context_rows_are_keyed_to_their_image() -> anyhow::Result<()> {
    let db = ingest(bids_example("asl001")).await?;

    let (rows, images): (i64, i64) = db.conn.query_row(
        "SELECT COUNT(*), COUNT(DISTINCT file_id) FROM asl_volumes",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    assert!(rows > 0, "the aslcontext rows should reach their image");
    assert_eq!(images, 1, "asl001 has one asl run");

    // The view exposes the ordinal under its declared axis, and in file order.
    let first: (i64, String) = db.conn.query_row(
        "SELECT volume_idx, volume_type FROM asl_volumes ORDER BY volume_idx LIMIT 1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    assert_eq!(first.0, 0);
    assert_eq!(first.1, "m0scan", "asl001's first volume is the M0 scan");

    // The image it keys on is the ASL series itself, not the tsv.
    let suffix: String = db.conn.query_row(
        "SELECT DISTINCT f.suffix FROM asl_volumes v JOIN all_files f USING (file_id)",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(suffix, "asl");
    Ok(())
}

/// `events` is the same relation seen at its oldest: ds114 ships root-level
/// `task-*_events.tsv` files that apply to every matching BOLD run, stored **once** and
/// reached through `file_associations`. That precedent is what says the diffusion rework is
/// a correction rather than a new design, and nothing pinned it.
///
/// `events` is declared `describes` with no `axis` and no `view`: its rows correspond to a
/// *time*, not to a position in the data file (hence `ordered: false`), and both sides of
/// the relation would want the name `events`.
#[tokio::test]
async fn root_level_events_are_stored_once_and_shared() -> anyhow::Result<()> {
    let db = ingest(bids_example("ds114")).await?;

    let (edges, targets): (i64, i64) = db.conn.query_row(
        "SELECT COUNT(*), COUNT(DISTINCT target_file_path) FROM file_associations \
         WHERE association_type = 'events'",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    assert!(edges > targets, "one events file serves many runs");

    // The rows themselves are keyed by the tsv, exactly as `bvals` is by the `.bval`.
    let (rows, files): (i64, i64) = db.conn.query_row(
        "SELECT COUNT(*), COUNT(DISTINCT file_id) FROM events",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    assert_eq!(files, targets, "stored once per events file, not per run");
    assert!(rows > 0);

    // And `events` gets no view, because it declares neither an axis nor a name for one.
    let view_exists: i64 = db.conn.query_row(
        "SELECT COUNT(*) FROM duckdb_views() WHERE view_name = 'events'",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(view_exists, 0);
    Ok(())
}

/// Re-indexing must not duplicate gradient rows: `bvals`/`bvecs` upsert on
/// `(file_id, row_idx)`, and `file_id` is content-derived, so a second run lands on the same
/// rows (docs/adr/0006 §4).
#[tokio::test]
async fn re_indexing_upserts_gradients() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path();
    fs::create_dir_all(root.join("sub-01/dwi"))?;
    fs::write(
        root.join("dataset_description.json"),
        r#"{"Name":"reindex gradients","BIDSVersion":"1.11.1"}"#,
    )?;
    fs::write(root.join("sub-01/dwi/sub-01_dwi.nii.gz"), b"")?;
    fs::write(root.join("sub-01/dwi/sub-01_dwi.bval"), "0 1000 2000")?;
    fs::write(
        root.join("sub-01/dwi/sub-01_dwi.bvec"),
        "0 1 0\n0 0 1\n1 0 0",
    )?;

    let db = ingest(root).await?;
    let before = shape(&db)?;
    // A second `bidslake index` run over the same root.
    common::ingest_inferred_into(&db, root).await?;
    assert_eq!(
        shape(&db)?,
        before,
        "a re-index upserts rather than doubling"
    );
    Ok(())
}
