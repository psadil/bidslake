//! Schema-augmentation (overlay) integration tests.
//!
//! Builds a tiny synthetic fMRIPrep-style *derivative* dataset and indexes it both
//! with and without the bundled `fmriprep` overlay, proving the overlay is
//! load-bearing: with it, a `desc-confounds_timeseries.tsv` becomes a first-class
//! typed table and the pipeline's non-BIDS `from`/`to`/`mode` transform entities are
//! parsed; without it, the confounds file is recorded `skipped` and no such table
//! exists.

mod common;

use bidslake::schema::{AppliedOverlay, Ingestion, Schema};
use common::{count, ingest_with_schema};
use rstest::rstest;
use std::fs;
use std::path::Path;

/// Write the synthetic derivative tree under `root`.
fn write_derivative_tree(root: &Path) {
    let func = root.join("sub-01/func");
    let anat = root.join("sub-01/anat");
    fs::create_dir_all(&func).unwrap();
    fs::create_dir_all(&anat).unwrap();

    fs::write(
        root.join("dataset_description.json"),
        r#"{"Name":"fMRIPrep test derivative","BIDSVersion":"1.11.1","DatasetType":"derivative","GeneratedBy":[{"Name":"fMRIPrep","Version":"23.2.0"}]}"#,
    )
    .unwrap();

    // Surface morphometry, named by measure the way fMRIPrep writes it. Undeclared until the
    // always-applied `bep011` overlay, and reachable now without any adapter being named.
    fs::write(anat.join("sub-01_hemi-L_thickness.shape.gii"), b"").unwrap();
    fs::write(anat.join("sub-01_hemi-R_thickness.shape.gii"), b"").unwrap();

    // A preprocessed BOLD scan + sidecar.
    fs::write(func.join("sub-01_task-rest_desc-preproc_bold.nii.gz"), b"").unwrap();
    // A second spatial variant of the *same* run. One confounds file describes both, which
    // is the many-to-many the association exists for: fMRIPrep emits several spaces (and
    // surface GIFTIs) per run and one confounds table covering all of them. Without this
    // file the fan-out would be 1:1 and the test could not tell a re-key from a copy.
    fs::write(
        func.join("sub-01_task-rest_space-MNI152NLin2009cAsym_desc-preproc_bold.nii.gz"),
        b"",
    )
    .unwrap();
    // `SiteSpecificNote` is not a BIDS metadata field, so it has no dedicated column
    // and lands in `other_data` — the custom metadata an undeclared-column policy
    // scoped to *other* files must leave alone.
    fs::write(
        func.join("sub-01_task-rest_desc-preproc_bold.json"),
        r#"{"RepetitionTime":2.0,"SkullStripped":true,"SiteSpecificNote":"keep me"}"#,
    )
    .unwrap();

    // A confounds timeseries: ordered rows (row N == volume N); first FD is n/a.
    // `a_comp_cor_00` stands in for the ~1,700 CompCor regressors the schema does not
    // declare — real files carry hundreds, which is what the undeclared-column policy
    // exists for.
    fs::write(
        func.join("sub-01_task-rest_desc-confounds_timeseries.tsv"),
        "trans_x\ttrans_y\ttrans_z\tframewise_displacement\tglobal_signal\ta_comp_cor_00\n\
         0.10\t0.20\t0.30\tn/a\t100.5\t0.001\n\
         0.11\t0.19\t0.31\t0.05\t100.6\t0.002\n\
         0.12\t0.18\t0.29\t0.04\t100.4\t0.003\n",
    )
    .unwrap();

    // The confounds sidecar: fMRIPrep ships one per run, describing every column.
    // Orphaned (no matching data file), so it reaches `sidecars` via
    // `promote_orphan_sidecars` — the same route as MRIQC's IQMs.
    fs::write(
        func.join("sub-01_task-rest_desc-confounds_timeseries.json"),
        r#"{"SamplingFrequency":0.5,"a_comp_cor_00":{"CumulativeVarianceExplained":0.1}}"#,
    )
    .unwrap();

    // A spatial transform whose from/to/mode entities are not in base BIDS.
    fs::write(
        anat.join("sub-01_from-T1w_to-MNI152NLin2009cAsym_mode-image_xfm.h5"),
        b"",
    )
    .unwrap();
}

fn fmriprep_overlay() -> AppliedOverlay {
    AppliedOverlay {
        source: "fmriprep".to_string(),
        content: bids_schema::overlay::bundled_overlay("fmriprep")
            .expect("bundled fmriprep overlay"),
    }
}

/// A BIDS-named surface map reads its concepts out of its own filename, so this needs no adapter
/// and no `--overlay`: it is what the always-applied vocabulary buys on an ordinary ingest. The
/// extension is the part worth asserting — `.shape.gii` is two dots, and a parser that split on
/// the last one would report `.gii` and a suffix of `thickness.shape`.
#[tokio::test]
async fn a_surface_map_indexes_under_its_measure_suffix() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    write_derivative_tree(dir.path());
    let db = common::ingest(dir.path()).await?;

    let row: String = db.conn.query_row(
        "SELECT suffix || ' ' || extension || ' ' || hemi FROM all_files \
         WHERE file_path LIKE '%hemi-L_thickness%'",
        [],
        |r| r.get(0),
    )?;

    assert_eq!(row, "thickness .shape.gii L");
    Ok(())
}

#[tokio::test]
async fn overlay_makes_confounds_a_typed_ordered_table() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    write_derivative_tree(dir.path());

    let schema = Schema::load_with_overlays(None, &[fmriprep_overlay()])?;
    let db = ingest_with_schema(dir.path(), schema).await?;

    // The new table exists with the typed confound columns.
    assert_eq!(
        count(&db, "fmriprep_confounds")?,
        3,
        "3 confound rows ingested"
    );
    let cols: Vec<String> = {
        let mut stmt = db.conn.prepare(
            "SELECT column_name FROM information_schema.columns \
             WHERE table_name = 'fmriprep_confounds' ORDER BY column_name",
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect::<Result<_, _>>()?
    };
    for expected in [
        "trans_x",
        "framewise_displacement",
        "global_signal",
        "row_idx",
    ] {
        assert!(
            cols.contains(&expected.to_string()),
            "missing column {expected}"
        );
    }

    // Rows preserve TSV line order (volume order), and the first FD is NULL (n/a).
    let ordered: Vec<(i64, f64, Option<f64>)> = {
        let mut stmt = db.conn.prepare(
            "SELECT row_idx, trans_x, framewise_displacement FROM fmriprep_confounds ORDER BY row_idx",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, f64>(1)?,
                r.get::<_, Option<f64>>(2)?,
            ))
        })?;
        rows.collect::<Result<_, _>>()?
    };
    assert_eq!(
        ordered.iter().map(|r| r.0).collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
    assert_eq!(ordered[0].1, 0.10, "trans_x of first volume");
    assert!(
        ordered[0].2.is_none(),
        "first framewise_displacement is n/a -> NULL"
    );

    // The transform's non-BIDS entities are parsed into generated scans columns.
    let (from, to, mode, suffix): (String, String, String, String) = db.conn.query_row(
        r#"SELECT "from", "to", "mode", suffix FROM all_files WHERE kind = 'data' AND "from" IS NOT NULL"#,
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?;
    assert_eq!(
        (from.as_str(), to.as_str(), mode.as_str(), suffix.as_str()),
        ("T1w", "MNI152NLin2009cAsym", "image", "xfm")
    );

    // The confounds file is *linked* to the images it describes, not just stored beside
    // them. Two edges, both to the one TSV: the native-space and MNI variants of one run.
    // (docs/adr/0003 — before this, consumers matched `sub`/`task`/`run` by hand.)
    let (edges, targets): (i64, i64) = db.conn.query_row(
        "SELECT COUNT(*), COUNT(DISTINCT target_file_path) FROM file_associations \
         WHERE association_type = 'fmriprep_confounds'",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    assert_eq!(edges, 2, "one edge per BOLD variant of the run");
    assert_eq!(targets, 1, "both point at the single confounds file");

    let dangling: i64 = db.conn.query_row(
        "SELECT COUNT(*) FROM file_associations \
         WHERE association_type = 'fmriprep_confounds' AND target_file_id IS NULL",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(dangling, 0);

    // The *view* over those edges is not the overlay's doing — it is declared by the
    // ingestion fragment, which this test deliberately does not apply. See
    // `adapter_keys_confounds_rows_to_every_image_of_the_run` for the other half.
    let view_exists: i64 = db.conn.query_row(
        "SELECT COUNT(*) FROM duckdb_views() WHERE view_name = 'timeseries'",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(
        view_exists, 0,
        "the overlay supplies the edges; the ingestion fragment supplies the view"
    );

    // The database is self-describing: overlay provenance is stamped.
    assert_eq!(count(&db, "bidslake_schema")?, 1);
    let (idx, source): (i32, String) = db.conn.query_row(
        "SELECT idx, source FROM bidslake_overlays ORDER BY idx LIMIT 1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    assert_eq!((idx, source.as_str()), (0, "fmriprep"));

    Ok(())
}

#[tokio::test]
async fn without_overlay_confounds_is_skipped() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    write_derivative_tree(dir.path());

    // Control: the *same* tree, indexed with the plain base schema.
    let db = ingest_with_schema(dir.path(), Schema::load(None)?).await?;

    let table_exists: i64 = db.conn.query_row(
        "SELECT COUNT(*) FROM information_schema.tables WHERE table_name = 'fmriprep_confounds'",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(
        table_exists, 0,
        "no fmriprep_confounds table without the overlay"
    );

    let status: String = db.conn.query_row(
        "SELECT status FROM file_registry WHERE file_path LIKE '%confounds%.tsv'",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(
        status, "skipped",
        "confounds tsv is skipped without the overlay"
    );

    // The *edges* are overlay-carried too, and inert on plain BIDS. `meta.associations` is
    // a schema document, so a base-schema catalog must not sprout a pipeline's relation —
    // and with no table to key, a `timeseries` view would have nothing to select from.
    let edges: i64 = db.conn.query_row(
        "SELECT COUNT(*) FROM file_associations WHERE association_type = 'fmriprep_confounds'",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(edges, 0, "no confounds edges without the overlay");
    let view_exists: i64 = db.conn.query_row(
        "SELECT COUNT(*) FROM duckdb_views() WHERE view_name = 'timeseries'",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(view_exists, 0, "no timeseries view without the overlay");

    // Every database embeds its effective schema, but an un-augmented one records no
    // overlay provenance (NULL digest, and no bidslake_overlays table).
    let digest_is_null: bool = db.conn.query_row(
        "SELECT overlay_digest IS NULL FROM bidslake_schema",
        [],
        |r| r.get(0),
    )?;
    assert!(digest_is_null, "un-augmented DB has no overlay digest");
    let has_overlays_tbl: i64 = db.conn.query_row(
        "SELECT COUNT(*) FROM information_schema.tables WHERE table_name = 'bidslake_overlays'",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(
        has_overlays_tbl, 0,
        "no bidslake_overlays table without overlays"
    );

    Ok(())
}

/// `undeclared: catalog` on a table drops its `other_data` column entirely, so the
/// columns the schema does not declare are never stored — while the declared ones
/// and the file's own registry record are untouched. This is a *column*
/// policy, not a read/skip policy: the file is still parsed.
#[tokio::test]
async fn undeclared_catalog_drops_the_overflow_column() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    write_derivative_tree(dir.path());

    let ingestion = Ingestion::from_sources(&[
        bids_schema::bundled_ingestion_source("base").expect("base ingestion"),
        r#"{ "IngestionSchemaVersion": "0.1.0",
             "tables": { "fmriprep_confounds": { "undeclared": "catalog" } } }"#,
    ])?;
    let schema = Schema::load_full(None, &[fmriprep_overlay()], ingestion, &[])?;
    let db = ingest_with_schema(dir.path(), schema).await?;

    let has_other_data: bool = db.conn.query_row(
        "SELECT count(*) > 0 FROM information_schema.columns \
         WHERE table_name = 'fmriprep_confounds' AND column_name = 'other_data'",
        [],
        |r| r.get(0),
    )?;
    assert!(
        !has_other_data,
        "a catalog table should have no other_data column at all"
    );

    // Declared columns still populate, and the rows are all there.
    let (rows, trans_x): (i64, Option<f64>) = db.conn.query_row(
        "SELECT count(*), min(trans_x) FROM fmriprep_confounds",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    assert_eq!(rows, 3, "the file is still read, just not hoarded");
    assert_eq!(trans_x, Some(0.10));

    // And it is still accounted for, so the file on disk remains findable.
    let status: String = db.conn.query_row(
        "SELECT status FROM file_registry WHERE file_path LIKE '%confounds_timeseries.tsv'",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(status, "ingested");

    // The name of what was not stored is recorded, so a user can discover it without
    // opening the file — losslessness by reference, cheaply.
    let names: Vec<String> = db
        .conn
        .prepare("SELECT name FROM tabular_undeclared_columns WHERE table_name = ?")?
        .query_map(["fmriprep_confounds"], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    assert_eq!(names, ["a_comp_cor_00"]);

    // Declared columns never appear there — it records what is *missing* from the
    // catalog, not the header.
    let has_declared: bool = db.conn.query_row(
        "SELECT count(*) > 0 FROM tabular_undeclared_columns WHERE name = 'trans_x'",
        [],
        |r| r.get(0),
    )?;
    assert!(!has_declared);
    Ok(())
}

/// `undeclaredWhen` scopes the policy to the files a selector matches. `sidecars` is
/// one table for *every* file in the catalog, so this is what lets a derivative's
/// 366 KB confounds sidecar be dropped while an ordinary BIDS sidecar in the same
/// database keeps its custom metadata.
#[tokio::test]
async fn undeclared_when_scopes_sidecars_per_file() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    write_derivative_tree(dir.path());

    let ingestion = Ingestion::from_sources(&[
        bids_schema::bundled_ingestion_source("base").expect("base ingestion"),
        r#"{ "IngestionSchemaVersion": "0.1.0",
             "tables": { "sidecars": { "undeclaredWhen": [
                 { "selectors": ["suffix == \"timeseries\"", "extension == \".json\""],
                   "undeclared": "catalog" } ] } } }"#,
    ])?;
    let schema = Schema::load_full(None, &[fmriprep_overlay()], ingestion, &[])?;
    let db = ingest_with_schema(dir.path(), schema).await?;

    // The confounds sidecar: its undeclared per-column descriptions are gone, but the
    // declared `SamplingFrequency` still reaches its typed column.
    let (other, sampling): (Option<String>, Option<f64>) = db.conn.query_row(
        r#"SELECT other_data::VARCHAR, "SamplingFrequency" FROM sidecars
           JOIN all_files USING (file_id) WHERE file_path LIKE '%confounds_timeseries.json'"#,
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    assert_eq!(other, None, "confounds sidecar overflow should be dropped");
    assert_eq!(
        sampling,
        Some(0.5),
        "declared fields are unaffected by the policy"
    );

    // The ordinary BOLD sidecar in the same database keeps its custom field — this is
    // the assertion that proves the scope is per-file, not per-table.
    let bold: Option<String> = db.conn.query_row(
        "SELECT other_data::VARCHAR FROM sidecars JOIN all_files USING (file_id) WHERE file_path LIKE '%preproc_bold.nii.gz'",
        [],
        |r| r.get(0),
    )?;
    let obj: serde_json::Value = serde_json::from_str(&bold.expect("bold sidecar has overflow"))?;
    assert_eq!(
        obj.get("SiteSpecificNote").and_then(|v| v.as_str()),
        Some("keep me"),
        "an unmatched file must keep its custom metadata: {obj}"
    );
    Ok(())
}

/// The bundled `fmriprep` adapter, end to end: its overlay makes confounds a typed
/// table and its ingestion fragment keeps that table (and the confounds sidecar) from
/// hoarding the ~1,800 columns the schema does not declare. This is what
/// `--adapter fmriprep` gets a user, with no hand-written policy.
#[tokio::test]
async fn bundled_fmriprep_adapter_catalogs_undeclared_columns() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    write_derivative_tree(dir.path());

    let ingestion = Ingestion::from_sources(&[
        bids_schema::bundled_ingestion_source("base").expect("base ingestion"),
        bids_schema::bundled_ingestion_source("fmriprep").expect("bundled fmriprep ingestion"),
    ])?;
    let schema = Schema::load_full(None, &[fmriprep_overlay()], ingestion, &[])?;
    let db = ingest_with_schema(dir.path(), schema).await?;

    // Confounds: declared columns kept, undeclared ones recorded but not stored.
    let has_other_data: bool = db.conn.query_row(
        "SELECT count(*) > 0 FROM information_schema.columns \
         WHERE table_name = 'fmriprep_confounds' AND column_name = 'other_data'",
        [],
        |r| r.get(0),
    )?;
    assert!(!has_other_data);
    assert_eq!(count(&db, "fmriprep_confounds")?, 3);
    let recorded: i64 = db.conn.query_row(
        "SELECT count(*) FROM tabular_undeclared_columns WHERE name = 'a_comp_cor_00'",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(recorded, 1);

    // The confounds sidecar loses its per-column dictionary...
    let confounds_sidecar: Option<String> = db.conn.query_row(
        "SELECT other_data::VARCHAR FROM sidecars JOIN all_files USING (file_id) WHERE file_path LIKE '%confounds_timeseries.json'",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(confounds_sidecar, None);

    // ...while an ordinary sidecar in the same dataset keeps its custom fields.
    let bold_sidecar: Option<String> = db.conn.query_row(
        "SELECT other_data::VARCHAR FROM sidecars JOIN all_files USING (file_id) WHERE file_path LIKE '%preproc_bold.nii.gz'",
        [],
        |r| r.get(0),
    )?;
    assert!(
        bold_sidecar.is_some_and(|s| s.contains("SiteSpecificNote")),
        "the policy is scoped to confounds, not to the whole table"
    );
    Ok(())
}

/// The full adapter — overlay (edges) plus ingestion fragment (the `describes` view) — makes
/// a confounds table readable *by volume of a given image*, which is the whole point of
/// docs/adr/0003. Row N of the TSV is volume N of every BOLD variant the run produced.
///
/// This has to be a synthetic fixture: the vendored `ds000001-fmriprep` confounds files are
/// zero bytes, so the corpus can prove the edges exist but never that the rows line up.
#[tokio::test]
async fn adapter_keys_confounds_rows_to_every_image_of_the_run() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    write_derivative_tree(dir.path());

    let ingestion = Ingestion::from_sources(&[
        bids_schema::bundled_ingestion_source("base").expect("base ingestion"),
        bids_schema::bundled_ingestion_source("fmriprep").expect("bundled fmriprep ingestion"),
    ])?;
    let schema = Schema::load_full(None, &[fmriprep_overlay()], ingestion, &[])?;
    let db = ingest_with_schema(dir.path(), schema).await?;

    let aligned: Vec<(String, i64, f64)> = {
        let mut stmt = db.conn.prepare(
            "SELECT f.file_path, t.volume_idx, t.trans_x \
             FROM timeseries t JOIN all_files f USING (file_id) \
             ORDER BY f.file_path, t.volume_idx",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        rows.collect::<Result<_, _>>()?
    };

    // 2 images x 3 volumes. The native-space variant sorts first (no `space-` entity).
    assert_eq!(aligned.len(), 6, "both BOLD variants see all three volumes");
    assert_eq!(
        aligned.iter().map(|r| r.1).collect::<Vec<_>>(),
        vec![0, 1, 2, 0, 1, 2]
    );
    assert_eq!(
        aligned.iter().map(|r| r.2).collect::<Vec<_>>(),
        vec![0.10, 0.11, 0.12, 0.10, 0.11, 0.12],
        "row N of the TSV is volume N of each image it describes"
    );
    assert!(
        aligned[3].0.contains("space-MNI152NLin2009cAsym"),
        "the second variant is the MNI one, not a duplicate of the first"
    );

    // Stored once, read twice: the fan-out belongs to the view, not to the table. This is
    // the property that makes an inherited describing file cheap — ds114's one `dwi.bval`
    // covers 20 images without 20 copies.
    assert_eq!(count(&db, "fmriprep_confounds")?, 3);
    let sources: i64 = db.conn.query_row(
        "SELECT COUNT(DISTINCT source_file_id) FROM timeseries",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(sources, 1, "the view records which file the rows came from");

    // `volume_idx`, not `row_idx`: the stored column records line order, the view records
    // what that line order means (the declaration's `axis`).
    let has_volume_idx: i64 = db.conn.query_row(
        "SELECT COUNT(*) FROM information_schema.columns \
         WHERE table_name = 'timeseries' AND column_name = 'volume_idx'",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(has_volume_idx, 1);
    Ok(())
}

#[test]
fn conflicting_overlay_is_rejected() {
    // An overlay that tries to *change* an existing base entity (subject's short
    // name) rather than add — additive-only merge must reject it.
    let overlay = AppliedOverlay {
        source: "bad".to_string(),
        content: serde_json::json!({
            "objects": { "entities": { "subject": { "name": "SUBJECT_RENAMED" } } }
        }),
    };
    let err = Schema::load_with_overlays(None, &[overlay]).unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("conflict") || msg.contains("additive"),
        "expected an additive-only conflict error, got: {msg}"
    );
}

/// fMRIPrep and QSIPrep both name their confounds file `*_desc-confounds_timeseries.tsv`,
/// and `TabularRule::identity_key` drops the non-identity `DatasetType` selector — so before
/// the two rules were narrowed by `datatype`, applying both adapters collapsed them into one
/// table and QSIPrep's rows landed in `fmriprep_confounds` while `qsiprep_confounds` stayed
/// empty. Adding the edges made that newly visible: a join through `qsiprep_confounds` would
/// have returned nothing while correctly-labelled edges pointed at rows in the other table.
///
/// `datatype` is bound at the `Tabular::route` call site, so one selector per rule separates
/// them completely. This pins that, and that each gets its own re-keying view.
/// The derivative tree with a diffusion run beside the functional one, indexed with both
/// pipelines' adapters applied — QSIPrep's confounds file is named exactly like fMRIPrep's.
async fn ingest_both_pipelines(dir: &Path) -> anyhow::Result<bidslake::db::BidsDb> {
    write_derivative_tree(dir);

    let dwi = dir.join("sub-01/dwi");
    fs::create_dir_all(&dwi).unwrap();
    fs::write(dwi.join("sub-01_desc-preproc_dwi.nii.gz"), b"").unwrap();
    fs::write(
        dwi.join("sub-01_desc-confounds_timeseries.tsv"),
        "trans_x\tframewise_displacement\n0.90\t0.5\n0.91\t0.6\n",
    )
    .unwrap();

    let ingestion = Ingestion::from_sources(&[
        bids_schema::bundled_ingestion_source("base").expect("base ingestion"),
        bids_schema::bundled_ingestion_source("fmriprep").expect("fmriprep ingestion"),
        bids_schema::bundled_ingestion_source("qsiprep").expect("qsiprep ingestion"),
    ])?;
    let qsiprep = AppliedOverlay {
        source: "qsiprep".to_string(),
        content: bids_schema::overlay::bundled_overlay("qsiprep").expect("bundled qsiprep overlay"),
    };
    let schema = Schema::load_full(None, &[fmriprep_overlay(), qsiprep], ingestion, &[])?;
    ingest_with_schema(dir, schema).await
}

#[tokio::test]
async fn fmriprep_and_qsiprep_confounds_do_not_collapse() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let db = ingest_both_pipelines(dir.path()).await?;

    // Each pipeline's rows land in its own table — 3 functional volumes, 2 diffusion ones.
    assert_eq!(count(&db, "fmriprep_confounds")?, 3);
    assert_eq!(count(&db, "qsiprep_confounds")?, 2);

    // ...and each is reachable by volume of its own image, through its own view.
    let bold: Vec<f64> = {
        let mut stmt = db
            .conn
            .prepare("SELECT trans_x FROM timeseries ORDER BY file_id, volume_idx LIMIT 3")?;
        stmt.query_map([], |r| r.get(0))?
            .collect::<Result<_, _>>()?
    };
    assert_eq!(bold, vec![0.10, 0.11, 0.12]);

    let (dwi_rows, dwi_images): (i64, i64) = db.conn.query_row(
        "SELECT COUNT(*), COUNT(DISTINCT file_id) FROM dwi_timeseries",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    assert_eq!((dwi_rows, dwi_images), (2, 1));

    let dwi_first: f64 = db.conn.query_row(
        "SELECT trans_x FROM dwi_timeseries WHERE volume_idx = 0",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(dwi_first, 0.90, "the dwi view reads the dwi file's rows");
    Ok(())
}

/// Both adapters ship `undeclared: catalog`, so neither table hoards the ~1,800 undeclared
/// regressors as per-row JSON (docs/adr/0004).
#[rstest]
#[case("fmriprep_confounds")]
#[case("qsiprep_confounds")]
#[tokio::test]
async fn a_confounds_table_has_no_other_data_column(#[case] table: &str) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let db = ingest_both_pipelines(dir.path()).await?;

    let has_other_data: bool = db.conn.query_row(
        "SELECT count(*) > 0 FROM information_schema.columns \
         WHERE table_name = ? AND column_name = 'other_data'",
        [table],
        |r| r.get(0),
    )?;

    assert!(!has_other_data, "{table} should have no other_data column");
    Ok(())
}

/// A recording table the schema declares columns for must keep them.
///
/// `motion` and `stim` are generated as *bare* tables (`file_id`, `row_idx`, `other_data`)
/// because base BIDS gives them no `rules.tabular_data` column rule. That set is a hardcoded
/// literal in the generator, so it does not notice when an overlay *does* declare columns for
/// one of them: the bare spec is generated second, and `table_definitions` is keyed by table
/// name, so the last writer wins and the typed definition is silently replaced.
///
/// Reachable on plain BIDS too — the `motion` group of `rules.tabular_data` already holds
/// `motionChannels`, so a future `motionColumns` beside it would land on the same name.
#[test]
fn an_overlay_may_declare_columns_for_a_recording_table() {
    let overlay = AppliedOverlay {
        source: "stimtest".to_string(),
        content: serde_json::json!({
            "objects": {
                "columns": {
                    "stim_signal__test": {
                        "name": "stim_signal",
                        "display_name": "Stimulus signal",
                        "description": "Stimulus intensity at this sample.\n",
                        "type": "number"
                    }
                }
            },
            "rules": {
                "tabular_data": {
                    "stimtest": {
                        "stim": {
                            "selectors": ["suffix == \"stim\""],
                            "additional_columns": "allowed",
                            "columns": { "stim_signal__test": "optional" }
                        }
                    }
                }
            }
        }),
    };

    let schema = Schema::load_with_overlays(None, &[overlay]).expect("overlay loads");
    let ddl = schema
        .get_create_sql("stim")
        .expect("the stim table is generated");
    assert!(
        ddl.contains("stim_signal"),
        "the overlay's declared column was dropped; `stim` was generated bare:\n{ddl}"
    );
}
