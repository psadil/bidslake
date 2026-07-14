//! Layout-adapter integration tests (FreeSurfer), exercising the three-schema pipeline:
//! a BIDS overlay (tables), a BEP-043 term map (projection), and an ingestion schema
//! (read/catalog policy). Builds a synthetic FreeSurfer `SUBJECTS_DIR` covering all three
//! subject-dir forms — `sub-01_ses-1`/`sub-01_ses-2` (session), `sub-02` (sessionless), bare
//! `03` (no `sub-` prefix), all matched by one PCRE mapping each — and indexes it with the
//! bundled `freesurfer` adapter.

mod common;

use common::{count, ingest, ingest_with_adapters};
use std::fs;
use std::path::Path;

const ASEG_STATS: &str = "\
# Title Segmentation Statistics
#
# Measure BrainSeg, BrainSegVol, Brain Segmentation Volume, 1200000.000000, mm^3
# Measure EstimatedTotalIntraCranialVol, eTIV, Estimated Total Intracranial Volume, 1500000.000000, mm^3
# Measure SurfaceHoles, SurfaceHoles, Total number of defect holes, 25, unitless
# ColHeaders  Index SegId NVoxels Volume_mm3 StructName normMean normStdDev normMin normMax normRange
  1   4   5000   5100.0  Left-Lateral-Ventricle   35.0  10.0  10  90  80
  2  43   4800   4900.5  Right-Lateral-Ventricle  36.0  11.0  12  92  80
  3  17   4200   4300.2  Left-Hippocampus         70.0   9.0  40 110  70
";

const APARC_STATS: &str = "\
# Table of FreeSurfer cortical parcellation anatomical statistics
#
# Measure Cortex, NumVert, Number of Vertices, 120000, unitless
# Measure Cortex, MeanThickness, Mean Thickness, 2.5, mm
# ColHeaders StructName NumVert SurfArea GrayVol ThickAvg ThickStd MeanCurv GausCurv FoldInd CurvInd
bankssts         1000  700  2000  2.5  0.5  0.100  0.020  15  0.9
superiorfrontal  5000 3500 11000  2.8  0.6  0.090  0.020  30  2.5
";

fn write(root: &Path, rel: &str, content: &[u8]) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

/// A synthetic FreeSurfer SUBJECTS_DIR covering all three subject-dir forms.
fn write_fs_tree(root: &Path) {
    write(root, "sub-01_ses-1/stats/aseg.stats", ASEG_STATS.as_bytes());
    write(
        root,
        "sub-01_ses-1/stats/lh.aparc.stats",
        APARC_STATS.as_bytes(),
    );
    write(
        root,
        "sub-01_ses-1/stats/rh.aparc.stats",
        APARC_STATS.as_bytes(),
    );
    write(
        root,
        "sub-01_ses-1/surf/lh.thickness",
        b"\xff\xff\xffbinary",
    );
    write(root, "sub-01_ses-1/mri/aseg.mgz", b"\xff\xffMGZ");
    write(
        root,
        "sub-01_ses-1/label/aparc.annot.ctab",
        b"0   Unknown                 0   0   0   0\n\
          1   Left-Cerebral-Exterior  70  130 180 0\n\
          17  Left-Hippocampus        220 216 20  0\n",
    );
    write(root, "sub-01_ses-2/stats/aseg.stats", ASEG_STATS.as_bytes());
    write(root, "sub-02/stats/aseg.stats", ASEG_STATS.as_bytes());
    write(root, "03/stats/aseg.stats", ASEG_STATS.as_bytes());
}

#[tokio::test]
async fn aseg_stats_are_read_typed_across_all_subject_dir_forms() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    write_fs_tree(dir.path());
    let db = ingest_with_adapters(dir.path(), &["freesurfer"]).await?;

    assert_eq!(count(&db, "freesurfer_aseg")?, 12, "12 aseg rows");
    let seg_only: i64 = db.conn.query_row(
        "SELECT COUNT(*) FROM freesurfer_aseg WHERE seg = 'aseg'",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(seg_only, 12, "materialized `seg` concept column");

    // One PCRE mapping resolved all three subject-dir forms.
    for (sub, ses, n) in [("01", Some("1"), 3), ("02", None, 3), ("03", None, 3)] {
        let got: i64 = match ses {
            Some(s) => db.conn.query_row(
                "SELECT COUNT(*) FROM freesurfer_aseg WHERE sub = ? AND ses = ?",
                duckdb::params![sub, s],
                |r| r.get(0),
            )?,
            None => db.conn.query_row(
                "SELECT COUNT(*) FROM freesurfer_aseg WHERE sub = ? AND ses IS NULL",
                duckdb::params![sub],
                |r| r.get(0),
            )?,
        };
        assert_eq!(got, n, "subject-dir form for sub={sub}");
    }

    // Typed values (Volume_mm3 DOUBLE, SegId BIGINT).
    let (seg_id, vol): (i64, f64) = db.conn.query_row(
        "SELECT SegId, Volume_mm3 FROM freesurfer_aseg \
         WHERE sub = '01' AND ses = '1' AND StructName = 'Left-Hippocampus'",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    assert_eq!(seg_id, 17);
    assert!((vol - 4300.2).abs() < 1e-6);
    Ok(())
}

#[tokio::test]
async fn aparc_and_measures() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    write_fs_tree(dir.path());
    let db = ingest_with_adapters(dir.path(), &["freesurfer"]).await?;

    assert_eq!(count(&db, "freesurfer_aparc")?, 4, "lh+rh × 2 regions");
    let (num_vert, thick, parc): (i64, f64, String) = db.conn.query_row(
        "SELECT NumVert, ThickAvg, parc FROM freesurfer_aparc \
         WHERE hemi = 'lh' AND StructName = 'bankssts'",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    assert_eq!((num_vert, parc.as_str()), (1000, "aparc"));
    assert!((thick - 2.5).abs() < 1e-6);

    // Measures: one row per stats file with `# Measure` lines (4 aseg + lh/rh aparc = 6).
    assert_eq!(count(&db, "freesurfer_measures")?, 6);
    let etiv: f64 = db.conn.query_row(
        "SELECT eTIV FROM freesurfer_measures WHERE sub = '01' AND ses = '1' AND eTIV IS NOT NULL",
        [],
        |r| r.get(0),
    )?;
    assert!((etiv - 1_500_000.0).abs() < 1e-3);
    Ok(())
}

#[tokio::test]
async fn catalog_files_land_in_scans_and_labels_join() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    write_fs_tree(dir.path());
    let db = ingest_with_adapters(dir.path(), &["freesurfer"]).await?;

    // Catalog: surf/mri files are registered in the standard `scans` table (left on disk).
    let surf: i64 = db.conn.query_row(
        "SELECT COUNT(*) FROM scans WHERE file_path LIKE '%surf/lh.thickness'",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(surf, 1, "surface cataloged in scans");
    let mri: i64 = db.conn.query_row(
        "SELECT COUNT(*) FROM scans WHERE file_path LIKE '%mri/aseg.mgz'",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(mri, 1, "volume cataloged in scans");

    // Labels (from the .ctab reader), joinable to aseg on seg_id.
    assert_eq!(count(&db, "freesurfer_labels")?, 3);
    let name: String = db.conn.query_row(
        "SELECT l.struct_name FROM freesurfer_aseg a \
         JOIN freesurfer_labels l ON a.SegId = l.seg_id \
         WHERE a.StructName = 'Left-Hippocampus' LIMIT 1",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(name, "Left-Hippocampus");

    // Self-describing: term-map and ingestion provenance are stamped.
    let tm: String = db.conn.query_row(
        "SELECT source FROM bidslake_term_maps ORDER BY idx LIMIT 1",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(tm, "freesurfer");
    let ing: i64 = db
        .conn
        .query_row("SELECT COUNT(*) FROM bidslake_ingestion", [], |r| r.get(0))?;
    assert_eq!(ing, 1);
    Ok(())
}

/// A dataset ingested through an adapter has no `dataset_description.json` — that is what
/// makes it non-BIDS — but it must still record a `root_uri`, because that is what turns a
/// stored dataset-relative `file_path` back into an openable URI for a client (e.g.
/// bidslake-py's `BidsFile.local_path`). Without the synthesized row its files are
/// unresolvable.
#[tokio::test]
async fn adapter_dataset_records_a_root_uri() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    write_fs_tree(dir.path());
    assert!(
        !dir.path().join("dataset_description.json").exists(),
        "the FreeSurfer fixture has no dataset_description.json — that is the point"
    );

    let db = ingest_with_adapters(dir.path(), &["freesurfer"]).await?;

    let rows: i64 = db
        .conn
        .query_row("SELECT COUNT(*) FROM dataset_description", [], |r| r.get(0))?;
    assert_eq!(rows, 1, "exactly one synthesized dataset_description row");

    let root: String = db
        .conn
        .query_row("SELECT root_uri FROM dataset_description", [], |r| r.get(0))?;
    assert!(
        root.starts_with("file://"),
        "root_uri should be a file:// URI, got {root}"
    );

    // It must actually resolve: root_uri + a stored file_path is a real file on disk.
    let file_path: String = db.conn.query_row(
        "SELECT file_path FROM scans WHERE file_path LIKE '%mri/aseg.mgz' LIMIT 1",
        [],
        |r| r.get(0),
    )?;
    let resolved = std::path::Path::new(root.trim_start_matches("file://")).join(&file_path);
    assert!(
        resolved.is_file(),
        "root_uri + file_path should resolve to a real file, got {}",
        resolved.display()
    );
    Ok(())
}

#[tokio::test]
async fn without_adapter_freesurfer_tables_absent() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    write_fs_tree(dir.path());
    let db = ingest(dir.path()).await?;
    let has: i64 = db.conn.query_row(
        "SELECT COUNT(*) FROM information_schema.tables WHERE table_name = 'freesurfer_aseg'",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(has, 0, "no freesurfer tables without an adapter");
    Ok(())
}
