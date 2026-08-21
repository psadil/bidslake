//! Layout-adapter integration tests (FreeSurfer), exercising the three-schema pipeline:
//! a BIDS overlay (tables), a BEP-043 term map (projection), and an ingestion schema
//! (read/catalog policy). Builds a synthetic FreeSurfer `SUBJECTS_DIR` covering all three
//! subject-dir forms — `sub-01_ses-1`/`sub-01_ses-2` (session), `sub-02` (sessionless), bare
//! `03` (no `sub-` prefix), all matched by one PCRE mapping each — and indexes it with the
//! bundled `freesurfer` adapter.

mod common;

use common::{count, ingest, ingest_with_adapters, write};
use rstest::rstest;
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
    write(root, "sub-01_ses-1/surf/rh.curv", b"\xff\xff\xffbinary");
    write(root, "sub-01_ses-1/surf/lh.sulc", b"\xff\xff\xffbinary");
    // No BEP-011 term: a registered sphere is not the sphere, and the `.crv` files beside
    // `?h.smoothwm` are curvature derivatives of it. Both must stay on the catch-all, which
    // states a datatype and no suffix.
    write(
        root,
        "sub-01_ses-1/surf/lh.sphere.reg",
        b"\xff\xff\xffbinary",
    );
    write(
        root,
        "sub-01_ses-1/surf/lh.smoothwm.K1.crv",
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
    // Bookkeeping subtrees a real `recon-all` writes: recognized by the term map (so the
    // validator does not call them unknown) but projecting no `datatype`, so no ingestion
    // rule claims them and they earn no `scans` row.
    write(root, "sub-02/scripts/recon-all.log", b"cmdline ...\n");
    write(root, "sub-02/touch/wmsegment.touch", b"");
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

/// One PCRE mapping resolves all three subject-dir forms, so each yields the same three
/// `aseg` rows regardless of how the directory was named.
#[rstest]
#[case::session("01", Some("1"))]
#[case::sessionless("02", None)]
#[case::bare_label("03", None)]
#[tokio::test]
async fn one_mapping_resolves_every_subject_dir_form(
    #[case] sub: &str,
    #[case] ses: Option<&str>,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    write_fs_tree(dir.path());
    let db = ingest_with_adapters(dir.path(), &["freesurfer"]).await?;

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

    assert_eq!(got, 3, "subject-dir form for sub={sub}");
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
         WHERE hemi = 'L' AND StructName = 'bankssts'",
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

/// The hemisphere reaches the catalog as a **BIDS** label, not as FreeSurfer's filename token.
///
/// `lh`/`rh` in a `hemi` column would join with nothing any BIDS-named producer wrote, so the
/// term map declares `hemi: L` / `hemi: R` per mapping rather than capturing the path. Asserting
/// the whole distinct set, rather than one value, is what catches the plausible half-failure:
/// both mappings projecting the same label.
#[tokio::test]
async fn a_hemisphere_projects_the_bids_label() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    write_fs_tree(dir.path());
    let db = ingest_with_adapters(dir.path(), &["freesurfer"]).await?;

    let labels: String = db.conn.query_row(
        "SELECT string_agg(DISTINCT hemi, ',' ORDER BY hemi) FROM freesurfer_aparc",
        [],
        |r| r.get(0),
    )?;

    assert_eq!(labels, "L,R");
    Ok(())
}

/// The whole point, stated as the query a user would actually write.
///
/// Two producers, two conventions, one catalog: fMRIPrep puts the measure in the filename
/// (`_hemi-L_thickness.shape.gii`, parsed), FreeSurfer puts it in the position
/// (`surf/lh.thickness`, projected by the term map). If they agree on the suffix, one predicate
/// reaches both and a caller never has to know which tool ran. If they ever stop agreeing, this
/// returns 1 instead of 2 and says so.
#[tokio::test]
async fn one_predicate_reaches_both_producers_thickness_maps() -> anyhow::Result<()> {
    let fs_dir = tempfile::tempdir()?;
    write_fs_tree(fs_dir.path());
    let fmriprep_dir = tempfile::tempdir()?;
    let anat = fmriprep_dir.path().join("sub-01/anat");
    std::fs::create_dir_all(&anat)?;
    std::fs::write(
        fmriprep_dir.path().join("dataset_description.json"),
        r#"{"Name":"fp","BIDSVersion":"1.11.1","DatasetType":"derivative"}"#,
    )?;
    std::fs::write(anat.join("sub-01_hemi-L_thickness.shape.gii"), b"")?;

    // Both roots are indexed with both adapters configured, which is what a caller accumulating
    // them into one catalog does — and is required, not stylistic: the registry's columns are
    // fixed by the run that creates the table, and a term map adds `projected` to them. A second
    // run configured without one writes a narrower row than the table holds and fails in the
    // appender (docs/adr/0006, *A catalog cannot gain `projected` short of a rebuild*).
    let db = bidslake::db::BidsDb::new(":memory:")?;
    let adapters = ["freesurfer", "fmriprep"];
    common::ingest_with_adapters_into(&db, fs_dir.path(), &adapters, Some("recon")).await?;
    common::ingest_with_adapters_into(&db, fmriprep_dir.path(), &adapters, Some("fmriprep"))
        .await?;

    let producers: String = db.conn.query_row(
        "SELECT string_agg(DISTINCT dataset_id, ' ' ORDER BY dataset_id) FROM all_files \
         WHERE suffix = 'thickness'",
        [],
        |r| r.get(0),
    )?;

    assert_eq!(producers, "fmriprep recon");
    Ok(())
}

/// The claim the shared vocabulary exists to make. FreeSurfer names this file positionally —
/// `surf/lh.thickness` carries no BIDS entity to parse — so the suffix can only come from the
/// term map's projection, and it has to be the same string fMRIPrep puts in
/// `_hemi-L_thickness.shape.gii` or the two producers do not join.
#[tokio::test]
async fn a_surface_measure_reaches_the_catalog_under_its_bids_suffix() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    write_fs_tree(dir.path());
    let db = ingest_with_adapters(dir.path(), &["freesurfer"]).await?;

    let found: String = db.conn.query_row(
        "SELECT string_agg(suffix || '/' || hemi, ' ' ORDER BY suffix) FROM all_files \
         WHERE file_path LIKE '%/surf/%' AND suffix IS NOT NULL",
        [],
        |r| r.get(0),
    )?;

    assert_eq!(found, "curv/R sulc/L thickness/L");
    Ok(())
}

/// The other half, and the one that would rot silently. `recon-all` writes ~90 files into
/// `surf/` and only nine have a BIDS term; every other name shares a prefix with one of them.
/// A mapping that claimed `?h.sphere.reg` as `sphere` would put a wrong suffix in the catalog
/// rather than leave a right one out, which is the worse failure.
#[tokio::test]
async fn a_surface_file_with_no_bids_term_is_cataloged_without_one() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    write_fs_tree(dir.path());
    let db = ingest_with_adapters(dir.path(), &["freesurfer"]).await?;

    let unnamed: i64 = db.conn.query_row(
        "SELECT COUNT(*) FROM all_files WHERE datatype IS NOT NULL AND suffix IS NULL \
         AND file_path LIKE '%/surf/%'",
        [],
        |r| r.get(0),
    )?;

    assert_eq!(
        unnamed, 2,
        "lh.sphere.reg and lh.smoothwm.K1.crv stay unnamed"
    );
    Ok(())
}

#[tokio::test]
async fn catalog_files_land_in_scans_and_labels_join() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    write_fs_tree(dir.path());
    let db = ingest_with_adapters(dir.path(), &["freesurfer"]).await?;

    // Catalog: surf/mri files are registered in the standard `scans` table (left on disk).
    let surf: i64 = db.conn.query_row(
        "SELECT COUNT(*) FROM all_files WHERE datatype IS NOT NULL AND file_path LIKE '%surf/lh.thickness'",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(surf, 1, "surface cataloged in scans");
    let mri: i64 = db.conn.query_row(
        "SELECT COUNT(*) FROM all_files WHERE datatype IS NOT NULL AND file_path LIKE '%mri/aseg.mgz'",
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
/// bidslake-py's `BidsFile.local_path`). Without a `dataset_roots` row its files are
/// unresolvable; without the synthesized `dataset_description` row it is absent from
/// `lake.datasets()` and from the wide `files` view.
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
        .query_row("SELECT root_uri FROM dataset_roots", [], |r| r.get(0))?;
    assert!(
        root.starts_with("file://"),
        "root_uri should be a file:// URI, got {root}"
    );

    // It must actually resolve: root_uri + a stored file_path is a real file on disk.
    let file_path: String = db.conn.query_row(
        "SELECT file_path FROM all_files WHERE datatype IS NOT NULL AND file_path LIKE '%mri/aseg.mgz' LIMIT 1",
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

/// A term map states what a path denotes; the file registry must answer accordingly.
///
/// Cataloged files get no reader, so their `FileFacts` used to pick an ingestion rule and
/// then be dropped — leaving `scans` rows whose `datatype` was NULL even though the term
/// map declares `anat` for every mapping. The projection is now stored and the generated
/// concept columns fall back from it, so a path that carries none of its concepts in its
/// name still reads as what it is.
#[tokio::test]
async fn cataloged_projection_reaches_the_registry() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    write_fs_tree(dir.path());
    let db = ingest_with_adapters(dir.path(), &["freesurfer"]).await?;

    // `mri/aseg.mgz` has no BIDS entity in its name: `seg`, `datatype` and `suffix` can
    // only come from the projection.
    let (sub, ses, seg, datatype, suffix, modality): (
        String,
        String,
        String,
        String,
        String,
        String,
    ) = db.conn.query_row(
        "SELECT sub, ses, seg, datatype, suffix, modality FROM all_files \
         WHERE file_path LIKE '%sub-01_ses-1/mri/aseg.mgz'",
        [],
        |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        },
    )?;
    assert_eq!(
        (
            sub.as_str(),
            ses.as_str(),
            seg.as_str(),
            datatype.as_str(),
            suffix.as_str()
        ),
        ("01", "1", "aseg", "anat", "dseg")
    );
    // `modality` is a CASE over `datatype`, so it only resolves if a generated column can
    // read another generated column that itself consulted the projection.
    assert_eq!(
        modality, "mri",
        "modality chains off the projected datatype"
    );

    // Every cataloged file carries its term map's `datatype`, not just the segmentations.
    // The fs_stats ingestion rule selects on suffix alone, so a mapping that forgot its
    // `datatype` would still be read — and be visible only here, as a keeper that a
    // `datatype IS NOT NULL` query can no longer find. The keeper directories are spelled
    // out (mri/surf/stats/label) so the aparc and label mappings are covered, not only the
    // files whose rows land in `freesurfer_aseg`.
    let missing: i64 = db.conn.query_row(
        "SELECT COUNT(*) FROM all_files f \
         WHERE f.datatype IS NULL \
           AND (f.file_path LIKE '%/mri/%' OR f.file_path LIKE '%/surf/%' \
                OR f.file_path LIKE '%/stats/%' OR f.file_path LIKE '%/label/%' \
                OR EXISTS (SELECT 1 FROM freesurfer_aseg t WHERE t.file_id = f.file_id))",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(missing, 0, "every projected file records its datatype");
    Ok(())
}

/// Only the file registry can receive a projection, so only it pays for one.
///
/// `file_based` is true of every per-row tabular table as well as `scans`, so keying the
/// fallback on it put an always-NULL `projected` column on 24 tables and charged each of
/// their concept columns a COALESCE for a value that `ingest_projected` can never write
/// there — those tables are reached by the tabular readers instead.
#[tokio::test]
async fn only_the_file_registry_carries_a_projection() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    write_fs_tree(dir.path());
    let db = ingest_with_adapters(dir.path(), &["freesurfer"]).await?;

    // Asserted as a set rather than a list: the registry's own name and the surfaces built
    // over it are an implementation detail in flux (docs/adr/0006), but *which* things may
    // carry a projection is not — the registry, and whatever views expose it.
    let mut stmt = db.conn.prepare(
        "SELECT table_name FROM information_schema.columns WHERE column_name = 'projected' \
         ORDER BY table_name",
    )?;
    let carriers: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<Result<_, _>>()?;
    const REGISTRY_SURFACES: [&str; 2] = ["all_files", "file_registry"];
    assert_eq!(
        carriers, REGISTRY_SURFACES,
        "only the registry and its views may carry a projection"
    );

    // ...and **no base table at all** pays the COALESCE. It used to be on `scans`' generated
    // columns; the concept columns are now select items of the `all_files` view, so the cost
    // is paid once, on read, by whoever queries the view. `duckdb_tables()` excludes views,
    // which is exactly the point of asserting against it.
    let mut stmt = db.conn.prepare(
        "SELECT table_name FROM duckdb_tables() \
         WHERE sql LIKE '%json_extract_string(projected%' ORDER BY table_name",
    )?;
    let wrapped: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<Result<_, _>>()?;
    assert!(
        wrapped.is_empty(),
        "no base table should carry a projection fallback: {wrapped:?}"
    );
    Ok(())
}

/// The projection must not change what a BIDS-named file means. With an adapter active
/// every concept column is wrapped in a COALESCE, so this pins the fallback: a file the
/// term map does not claim still reads its concepts off the path.
#[tokio::test]
async fn bids_named_files_still_read_concepts_from_the_path() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    write_fs_tree(dir.path());
    // Not a FreeSurfer path — no term-map mapping claims it, so `projected` stays NULL.
    write(
        dir.path(),
        "sub-01/func/sub-01_task-rest_run-01_bold.nii.gz",
        b"\x00nii",
    );
    let db = ingest_with_adapters(dir.path(), &["freesurfer"]).await?;

    let (sub, task, run, datatype, suffix, projected_is_null): (
        String,
        String,
        String,
        String,
        String,
        bool,
    ) = db.conn.query_row(
        "SELECT sub, task, run, datatype, suffix, projected IS NULL FROM all_files \
         WHERE file_path LIKE '%_task-rest_run-01_bold.nii.gz'",
        [],
        |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        },
    )?;
    assert_eq!(
        (
            sub.as_str(),
            task.as_str(),
            run.as_str(),
            datatype.as_str(),
            suffix.as_str()
        ),
        ("01", "rest", "01", "func", "bold"),
        "concepts still parsed out of a BIDS filename"
    );
    assert!(
        projected_is_null,
        "a file no term map claims stores no projection"
    );
    Ok(())
}

/// With no term map configured there is nothing to project, so the registry keeps exactly
/// the DDL it had before this existed — no `projected` column and no COALESCE on any
/// concept column. That is what keeps a plain BIDS ingest paying nothing for a feature it
/// cannot use, and keeps the generated Python types stable.
#[tokio::test]
async fn plain_bids_registry_has_no_projection_column() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    write_fs_tree(dir.path());
    let db = ingest(dir.path()).await?;

    let has_column: i64 = db.conn.query_row(
        "SELECT COUNT(*) FROM information_schema.columns \
         WHERE table_name = 'scans' AND column_name = 'projected'",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(has_column, 0, "no projection column without a term map");

    let coalesced: i64 = db.conn.query_row(
        "SELECT COUNT(*) FROM duckdb_tables() \
         WHERE table_name = 'scans' AND sql LIKE '%json_extract_string(projected%'",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(coalesced, 0, "no concept column consults a projection");
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

/// A `recon-all` filename carries no BIDS entities — the subject is the *directory*. The
/// implicit-participant insert used to be gated on filename entities, so `participants` came
/// back empty for every adapter dataset even though `scans.sub` was populated, and any
/// `participants` ⋈ `scans` join silently dropped the whole dataset. The subject a term map
/// projects now registers the entity, exactly as a BIDS `sub-` prefix does.
#[tokio::test]
async fn projected_subjects_register_as_participants() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    write_fs_tree(dir.path());
    let db = ingest_with_adapters(dir.path(), &["freesurfer"]).await?;

    // All three subject-dir forms, normalized to the `sub-` spelling `participants` uses.
    let mut got: Vec<String> = db
        .conn
        .prepare("SELECT participant_id FROM participants ORDER BY 1")?
        .query_map([], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    got.sort();
    assert_eq!(got, vec!["sub-01", "sub-02", "sub-03"], "participants");

    // The session dir form registers its sessions against the right subject.
    let sessions: Vec<(String, String)> = db
        .conn
        .prepare("SELECT participant_id, session_id FROM sessions ORDER BY 1, 2")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;
    assert_eq!(
        sessions,
        vec![
            ("sub-01".to_string(), "ses-1".to_string()),
            ("sub-01".to_string(), "ses-2".to_string()),
        ],
        "sessions"
    );

    // The point of the fix: the join is total. Every cataloged file resolves to a participant.
    let orphans: i64 = db.conn.query_row(
        "SELECT COUNT(*) FROM all_files s \
         WHERE s.datatype IS NOT NULL AND s.sub IS NOT NULL \
           AND NOT EXISTS (SELECT 1 FROM participants p \
                           WHERE p.dataset_id = s.dataset_id \
                             AND p.participant_id = 'sub-' || s.sub)",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(orphans, 0, "every scans.sub resolves to a participants row");
    Ok(())
}

/// The bookkeeping subtrees are recognized without being cataloged: `scripts/` and `touch/`
/// project no `datatype`, so no ingestion rule claims them (`bids.rs`: "recognized but no
/// ingestion rule -> leave it alone"). Recognition is what keeps `bids-validator-rs` from
/// reporting them as unknown; it is not a licence to fill `scans` with logs.
#[rstest]
#[case("%scripts/%")]
#[case("%touch/%")]
#[tokio::test]
async fn recognized_bookkeeping_files_are_not_cataloged(
    #[case] pattern: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    write_fs_tree(dir.path());
    let db = ingest_with_adapters(dir.path(), &["freesurfer"]).await?;

    // `datatype` alone would be tautological here — the scripts/touch mappings project
    // none, whatever the ingestion decides — so `status` carries the sensitivity: a rule
    // that started cataloging (or reading) these files would stamp `on_disk`/`ingested`.
    let n: i64 = db.conn.query_row(
        "SELECT COUNT(*) FROM all_files \
         WHERE (datatype IS NOT NULL OR status IS NOT NULL) AND file_path LIKE ?",
        [pattern],
        |r| r.get(0),
    )?;

    assert_eq!(n, 0, "{pattern} must not be cataloged");
    Ok(())
}

/// ...but the subject they belong to is still registered, so a subject whose only files are
/// bookkeeping does not vanish from `participants`.
#[tokio::test]
async fn a_subject_with_only_bookkeeping_files_is_still_a_participant() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    write_fs_tree(dir.path());
    let db = ingest_with_adapters(dir.path(), &["freesurfer"]).await?;

    let n: i64 = db.conn.query_row(
        "SELECT COUNT(*) FROM participants WHERE participant_id = 'sub-02'",
        [],
        |r| r.get(0),
    )?;

    assert_eq!(n, 1);
    Ok(())
}

/// Re-indexing an adapter dataset must rebuild its reader tables, not append to them.
///
/// These tables are per-row and so carry no primary key, which makes this the quiet failure
/// mode: a second ingest does not error, it doubles every table — and a third triples it. The
/// erroring cases (`sidecars`, `file_associations`, `diffusion`) at least announce themselves.
#[tokio::test]
async fn reindexing_an_adapter_dataset_does_not_duplicate_reader_rows() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    write_fs_tree(dir.path());

    let tables = [
        "freesurfer_aseg",
        "freesurfer_aparc",
        "freesurfer_measures",
        "freesurfer_labels",
        // The registry rather than `scans`: a recon-all tree ships no `scans.tsv`, and since
        // docs/adr/0006 `scans` is that file's satellite, so it is legitimately empty here.
        "file_registry",
        "participants",
    ];
    let snapshot = |db: &bidslake::db::BidsDb| -> anyhow::Result<Vec<(String, i64)>> {
        tables
            .iter()
            .map(|t| Ok((t.to_string(), count(db, t)?)))
            .collect()
    };

    let db = common::ingest_with_adapters_as(dir.path(), &["freesurfer"], "fs").await?;
    let first = snapshot(&db)?;
    assert!(
        first.iter().all(|(_, n)| *n > 0),
        "need rows in every table to compare: {first:?}"
    );

    // Delete some of one table's rows so this also catches a re-ingest that writes nothing.
    db.conn
        .execute("DELETE FROM freesurfer_aseg WHERE sub = '02'", [])?;
    assert_ne!(snapshot(&db)?, first, "the delete must remove rows");

    common::ingest_with_adapters_into(&db, dir.path(), &["freesurfer"], Some("fs")).await?;
    assert_eq!(
        snapshot(&db)?,
        first,
        "a re-index must restore the deleted rows and duplicate none"
    );
    Ok(())
}
