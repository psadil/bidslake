//! Layout-adapter integration tests for FSL's FEAT/MELODIC/FIX output tree.
//!
//! A FEAT directory is named after the BIDS stem of the run it was built from
//! (`sub-01_ses-V1_task-rest_run-01_desc-preproc_bold/`), and everything inside it is
//! identified by *position* rather than by name — `reg/highres.nii.gz`,
//! `filtered_func_data.ica/melodic_mix`. So the unit's entities come from the directory
//! and each file's role comes from the projection.
//!
//! The tree is mostly scratch: a real FIX run leaves ~230 intermediates in `fix/` alone,
//! which is why the ingestion fragment's job here is mainly to `ignore`.

mod common;

use bidslake::db::BidsDb;
use common::{ingest_with_adapters, ingest_with_adapters_into, write};
use rstest::rstest;
use std::path::Path;

const UNIT: &str = "sub-01_ses-V1_task-rest_run-01_desc-preproc_bold";
/// A second unit, sessionless and runless, to pin the optional groups in the templates.
const BARE: &str = "sub-02_task-cuff_desc-preproc_bold";

/// Where the motion parameters sit inside a unit.
const MCF_PAR_PATH: &str = "mc/prefiltered_func_data_mcf.par";

/// Three volumes of motion in `mcflirt`'s own output format: fields separated by two spaces
/// with a trailing pair before the newline, values at the default `ostream` precision of six
/// significant figures (so a small one prints in scientific notation), and rotations — the
/// first three, in radians — two orders of magnitude smaller than the translations that follow
/// in mm. That last part is the point: a read that swapped the two halves would fail here
/// rather than merely look odd.
const MCF_PAR: &[u8] = b"-0.00123456  0.00234567  -1.23456e-05  0.123456  -0.234567  1.23456  \n\
                         0.000234567  -0.00345678  2.34567e-05  -0.345678  0.456789  -2.34567  \n\
                         0  0  0  0  0  0  \n";

fn write_feat_tree(root: &Path) {
    for (rel, body) in [
        // -- the outputs that matter -------------------------------------------------
        ("filtered_func_data.nii.gz", &b"nii"[..]),
        ("filtered_func_data_clean.nii.gz", b"nii"),
        ("filtered_func_data_clean_vn.nii.gz", b"nii"),
        ("mask.nii.gz", b"nii"),
        ("filtered_func_data.ica/melodic_mix", b"1 2\n3 4\n"),
        ("filtered_func_data.ica/melodic_FTmix", b"1 2\n3 4\n"),
        (
            "filtered_func_data.ica/melodic_ICstats",
            b"9.1 2.3\n8.2 1.9\n",
        ),
        ("filtered_func_data.ica/melodic_IC.nii.gz", b"nii"),
        ("filtered_func_data.ica/melodic_oIC.nii.gz", b"nii"),
        ("filtered_func_data.ica/mean.nii.gz", b"nii"),
        ("fix/features.csv", b"a,b\n1,2\n"),
        (MCF_PAR_PATH, MCF_PAR),
        ("fix4melview_UKBiobank_thr1.txt", b"1, Signal\n"),
        ("fix4melview_UKBiobank_thr1_psadil.txt", b"1, Signal\n"),
        // A second automatic labelling, from a training set whose name carries an
        // underscore. 7 of the 9 models FSL ships are spelled this way, so this is the
        // common case rather than the exotic one.
        ("fix4melview_HCP25_hp2000_thr10.txt", b"1, Signal\n"),
        ("reg/example_func.nii.gz", b"nii"),
        ("reg/highres.nii.gz", b"nii"),
        ("reg/standard.nii.gz", b"nii"),
        ("reg/highres_pveseg.nii.gz", b"nii"),
        ("reg/example_func2highres.mat", b"1 0 0 0\n"),
        ("reg/highres2example_func.mat", b"1 0 0 0\n"),
        ("reg/highres2standard.mat", b"1 0 0 0\n"),
        ("reg/standard2highres.mat", b"1 0 0 0\n"),
        ("reg/highres2standard_warp.nii.gz", b"nii"),
        ("reg/example_func2standard_warp.nii.gz", b"nii"),
        // -- scratch: everything below must be ignored --------------------------------
        ("fix/icmap0.nii.gz", b"nii"),
        ("fix/edge1.nii.gz", b"nii"),
        ("mc/prefiltered_func_data_mcf_conf.nii.gz", b"nii"),
        ("filtered_func_data.ica/melodic_pcaD", b"junk"),
        ("filtered_func_data.ica/eigenvalues_percent", b"junk"),
        ("filtered_func_data.ica/Noise__inv.nii.gz", b"nii"),
        ("filtered_func_data.ica/log.txt", b"log"),
        ("pyfix.log", b"log"),
    ] {
        write(root, &format!("{UNIT}/{rel}"), body);
    }
    write(root, &format!("{BARE}/mask.nii.gz"), b"nii");
    write(root, &format!("{BARE}/fix/icmap0.nii.gz"), b"nii");
}

/// The same tree with a different motion file, for the cases where the file's *contents* are
/// what is under test.
fn write_feat_tree_with_par(root: &Path, par: &[u8]) {
    write_feat_tree(root);
    write(root, &format!("{UNIT}/{MCF_PAR_PATH}"), par);
}

/// Each FEAT slot is reachable by what it *is*, not by where it sits.
#[rstest]
#[case("bold", "clean", 1)]
#[case("bold", "filtered", 1)]
#[case("bold", "cleanvn", 1)]
// The mask is the one slot both units write, so it is the only count above one.
#[case("mask", "brain", 2)]
#[case("mixing", "MELODIC", 1)]
#[case("spectrum", "MELODIC", 1)]
#[case("metrics", "MELODIC", 1)]
#[case("metrics", "fix", 1)]
#[case("timeseries", "motion", 1)]
#[case("components", "IC", 1)]
#[case("components", "oIC", 1)]
#[case("boldref", "exfunc", 1)]
#[case("boldref", "mean", 1)]
#[case("dseg", "pveseg", 1)]
#[case("T1w", "standard", 1)]
#[case("T1w", "brain", 1)]
// Two automatic labellings (one per training set), one hand-edited.
#[case("classification", "auto", 2)]
#[case("classification", "manual", 1)]
#[tokio::test]
async fn feat_roles_are_projected_onto_bids_concepts(
    #[case] suffix: &str,
    #[case] desc: &str,
    #[case] want: i64,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    write_feat_tree(dir.path());
    let db = ingest_with_adapters(dir.path(), &["feat"]).await?;

    let got: i64 = db.conn.query_row(
        "SELECT COUNT(*) FROM all_files WHERE datatype IS NOT NULL AND suffix = ? AND \"desc\" = ?",
        duckdb::params![suffix, desc],
        |r| r.get(0),
    )?;

    assert_eq!(got, want, "suffix={suffix} desc={desc}");
    Ok(())
}

/// The unit's entities come from the directory name, not from any file inside it.
#[tokio::test]
async fn unit_entities_come_from_the_directory_name() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    write_feat_tree(dir.path());
    let db = ingest_with_adapters(dir.path(), &["feat"]).await?;

    let (sub, ses, task, run): (String, String, String, String) = db.conn.query_row(
        "SELECT sub, ses, task, run FROM all_files WHERE datatype IS NOT NULL AND suffix = 'mixing'",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?;

    assert_eq!(
        (sub.as_str(), ses.as_str(), task.as_str(), run.as_str()),
        ("01", "V1", "rest", "01")
    );
    Ok(())
}

/// ...and the optional groups in the templates hold, so a directory naming neither a
/// session nor a run still resolves rather than failing to match.
#[tokio::test]
async fn a_sessionless_runless_unit_still_resolves() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    write_feat_tree(dir.path());
    let db = ingest_with_adapters(dir.path(), &["feat"]).await?;

    let bare: i64 = db.conn.query_row(
        "SELECT COUNT(*) FROM all_files WHERE datatype IS NOT NULL AND sub = '02' AND ses IS NULL AND run IS NULL \
         AND task = 'cuff' AND suffix = 'mask'",
        [],
        |r| r.get(0),
    )?;

    assert_eq!(bare, 1, "sessionless/runless FEAT dir still resolves");
    Ok(())
}

/// FSL names transforms `<from>2<to>`, which is exactly the `from`/`to` pair BIDS
/// derivatives use — so a FEAT registration directory becomes queryable by direction
/// rather than by filename, with the affine and the warp distinguished by extension.
#[rstest]
#[case("exfunc", "highres", ".mat")]
#[case("highres", "exfunc", ".mat")]
#[case("highres", "standard", ".mat")]
#[case("standard", "highres", ".mat")]
// The same direction as an affine above, told apart by extension alone.
#[case("highres", "standard", ".nii.gz")]
#[case("exfunc", "standard", ".nii.gz")]
#[tokio::test]
async fn registration_transforms_carry_from_and_to(
    #[case] from: &str,
    #[case] to: &str,
    #[case] ext: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    write_feat_tree(dir.path());
    let db = ingest_with_adapters(dir.path(), &["feat"]).await?;

    let got: i64 = db.conn.query_row(
        "SELECT COUNT(*) FROM all_files WHERE datatype IS NOT NULL AND suffix = 'xfm' \
         AND \"from\" = ? AND \"to\" = ? AND extension = ? AND mode = 'image'",
        duckdb::params![from, to, ext],
        |r| r.get(0),
    )?;

    assert_eq!(got, 1, "{from} -> {to} ({ext})");
    Ok(())
}

/// A hand-edited classification is the scientifically valuable artifact in this tree, and it
/// differs from the automatic one only by a trailing `_<rater>`. BIDS has no entity for a
/// rater, so the distinction lands in `desc` instead: `desc-manual` is a reviewed unit, and
/// its absence is what marks one as not yet reviewed. That is a closed two-value vocabulary,
/// so the query needs no advance knowledge of the site's rater labels.
#[tokio::test]
async fn desc_separates_hand_classification_from_automatic() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    write_feat_tree(dir.path());
    let db = ingest_with_adapters(dir.path(), &["feat"]).await?;

    let reviewed: i64 = db.conn.query_row(
        "SELECT COUNT(*) FROM all_files WHERE datatype IS NOT NULL AND suffix = 'classification' \
         AND \"desc\" = 'manual'",
        [],
        |r| r.get(0),
    )?;
    let automatic: i64 = db.conn.query_row(
        "SELECT COUNT(*) FROM all_files WHERE datatype IS NOT NULL AND suffix = 'classification' \
         AND \"desc\" = 'auto'",
        [],
        |r| r.get(0),
    )?;

    assert_eq!((reviewed, automatic), (1, 2));
    Ok(())
}

/// The training set and the threshold stay in the filename and reach no entity, because
/// neither can be one: a BIDS label is alphanumeric, and 7 of the 9 models FSL ships spell
/// their name with an underscore. Matching the model name *without* capturing it is what
/// makes those seven classify — bound to `desc`, the alphanumeric capture matched none of
/// them, so the common case was the unrecognized one.
#[tokio::test]
async fn an_underscored_training_set_still_classifies() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    write_feat_tree(dir.path());
    let db = ingest_with_adapters(dir.path(), &["feat"]).await?;

    let got: i64 = db.conn.query_row(
        "SELECT COUNT(*) FROM all_files WHERE datatype IS NOT NULL AND suffix = 'classification' \
         AND \"desc\" = 'auto' AND file_path LIKE '%/fix4melview_HCP25_hp2000_thr10.txt'",
        [],
        |r| r.get(0),
    )?;

    assert_eq!(got, 1);
    Ok(())
}

/// Most of a FEAT tree is intermediates. They must be recognized (so they are not treated
/// as stray BIDS files) and then dropped — otherwise a single run contributes hundreds of
/// meaningless registry rows.
#[rstest]
#[case("%/fix/icmap0.nii.gz")]
#[case("%/fix/edge1.nii.gz")]
#[case("%/mc/prefiltered_func_data_mcf_conf.nii.gz")]
#[case("%/melodic_pcaD")]
#[case("%/eigenvalues_percent")]
#[case("%/Noise__inv.nii.gz")]
#[case("%/filtered_func_data.ica/log.txt")]
#[case("%/pyfix.log")]
#[tokio::test]
async fn scratch_is_ignored_not_cataloged(#[case] pattern: &str) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    write_feat_tree(dir.path());
    let db = ingest_with_adapters(dir.path(), &["feat"]).await?;

    let got: i64 = db.conn.query_row(
        "SELECT COUNT(*) FROM all_files WHERE datatype IS NOT NULL AND file_path LIKE ?",
        duckdb::params![pattern],
        |r| r.get(0),
    )?;

    assert_eq!(got, 0, "scratch should not be cataloged: {pattern}");
    Ok(())
}

/// The motion parameters are a table, not a path: one row per volume, read by the `matrix`
/// engine from a file with no header at all.
#[tokio::test]
async fn motion_parameters_are_read_into_a_table() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    write_feat_tree(dir.path());
    let db = ingest_with_adapters(dir.path(), &["feat"]).await?;

    let rows: i64 = db
        .conn
        .query_row("SELECT COUNT(*) FROM feat_motion", [], |r| r.get(0))?;

    assert_eq!(rows, 3, "one row per volume in the .par");
    Ok(())
}

/// Nothing in the file says which column is which — `mcflirt` writes six bare numbers — so the
/// mapping comes from the order the schema declares, and that order is FSL's: three rotations
/// (radians), then three translations (mm). The reverse of fMRIPrep's confounds, which is
/// exactly why it is worth pinning value by value.
#[rstest]
#[case("rot_x", -0.00123456)]
#[case("rot_y", 0.00234567)]
#[case("rot_z", -1.23456e-05)]
#[case("trans_x", 0.123456)]
#[case("trans_y", -0.234567)]
#[case("trans_z", 1.23456)]
#[tokio::test]
async fn motion_columns_follow_mcflirts_order(
    #[case] column: &str,
    #[case] want: f64,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    write_feat_tree(dir.path());
    let db = ingest_with_adapters(dir.path(), &["feat"]).await?;

    let got: f64 = db.conn.query_row(
        &format!("SELECT {column} FROM feat_motion WHERE row_idx = 0"),
        [],
        |r| r.get(0),
    )?;

    assert!((got - want).abs() < 1e-12, "{column}: {got} != {want}");
    Ok(())
}

/// `row_idx` is the volume ordinal, so it runs densely from zero in file order — which is what
/// makes the table joinable to the image frame by frame.
#[tokio::test]
async fn row_idx_is_the_volume_ordinal() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    write_feat_tree(dir.path());
    let db = ingest_with_adapters(dir.path(), &["feat"]).await?;

    let trace: Vec<(i64, f64)> = db
        .conn
        .prepare("SELECT row_idx, trans_z FROM feat_motion ORDER BY row_idx")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;

    assert_eq!(trace, vec![(0, 1.23456), (1, -2.34567), (2, 0.0)]);
    Ok(())
}

/// A line that is too short, or that is not numbers at all, becomes a NULL row rather than no
/// row. Dropping it would slide every later volume one frame earlier against the image the rows
/// describe — a silent error, where a hole in the trace is a visible one.
#[rstest]
#[case::too_short("1  2  3\n")]
#[case::not_numbers("not  a  number  at  all  here\n")]
#[tokio::test]
async fn a_malformed_line_keeps_its_ordinal(#[case] bad: &str) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let par = format!("1  2  3  4  5  6\n{bad}7  8  9  10  11  12\n");
    write_feat_tree_with_par(dir.path(), par.as_bytes());
    let db = ingest_with_adapters(dir.path(), &["feat"]).await?;

    let trace: Vec<Option<f64>> = db
        .conn
        .prepare("SELECT trans_z FROM feat_motion ORDER BY row_idx")?
        .query_map([], |r| r.get(0))?
        .collect::<Result<_, _>>()?;

    assert_eq!(trace, vec![Some(6.0), None, Some(12.0)]);
    Ok(())
}

/// Blank lines are not volumes. A trailing newline is the common case and must not add a row
/// of NULLs at the end of every motion trace.
#[tokio::test]
async fn blank_lines_are_not_volumes() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    write_feat_tree_with_par(dir.path(), b"\n1  2  3  4  5  6\n\n7  8  9  10  11  12\n\n");
    let db = ingest_with_adapters(dir.path(), &["feat"]).await?;

    let rows: i64 = db
        .conn
        .query_row("SELECT COUNT(*) FROM feat_motion", [], |r| r.get(0))?;

    assert_eq!(rows, 2, "two data lines amongst three blanks");
    Ok(())
}

/// The rows carry no subject of their own — they key on the `.par` file, and the file's
/// concepts come from the registry (docs/adr/0006). So the question "whose motion is this?" is
/// a join, and it is the same join for any producer's timeseries table.
#[tokio::test]
async fn motion_rows_reach_their_subject_through_the_registry() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    write_feat_tree(dir.path());
    let db = ingest_with_adapters(dir.path(), &["feat"]).await?;

    let (sub, ses, task, run, desc): (String, String, String, String, String) = db.conn.query_row(
        "SELECT f.sub, f.ses, f.task, f.run, f.\"desc\" \
         FROM feat_motion m JOIN all_files f USING (file_id) WHERE m.row_idx = 0",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
    )?;

    assert_eq!(
        (
            sub.as_str(),
            ses.as_str(),
            task.as_str(),
            run.as_str(),
            desc.as_str()
        ),
        ("01", "V1", "rest", "01", "motion")
    );
    Ok(())
}

/// A file whose contents were read says so in the registry. It used to be cataloged, and the
/// status is the only place the difference shows.
#[tokio::test]
async fn a_read_par_is_recorded_ingested() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    write_feat_tree(dir.path());
    let db = ingest_with_adapters(dir.path(), &["feat"]).await?;

    let status: String = db.conn.query_row(
        "SELECT status FROM file_registry WHERE file_path LIKE ?",
        duckdb::params![format!("%/{MCF_PAR_PATH}")],
        |r| r.get(0),
    )?;

    assert_eq!(status, "ingested");
    Ok(())
}

/// `feat_motion` is per-row and so carries no primary key: a second ingest does not conflict,
/// it doubles the table. The `matrix` engine clears the file's rows first, and this is what
/// says so.
#[tokio::test]
async fn reindexing_does_not_duplicate_motion_rows() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    write_feat_tree(dir.path());
    let db = BidsDb::new(":memory:")?;
    ingest_with_adapters_into(&db, dir.path(), &["feat"], None).await?;
    ingest_with_adapters_into(&db, dir.path(), &["feat"], None).await?;

    let rows: i64 = db
        .conn
        .query_row("SELECT COUNT(*) FROM feat_motion", [], |r| r.get(0))?;

    assert_eq!(rows, 3, "a re-index replaces the file's rows");
    Ok(())
}

/// FEAT's motion parameters and fMRIPrep's confounds are the same measurement under different
/// producers, so they are declared from one vocabulary: `trans_x` is one column definition, and
/// a query for it needs no advance knowledge of which tool wrote the run.
#[rstest]
#[case("feat_motion")]
#[case("fmriprep_confounds")]
#[tokio::test]
async fn trans_x_is_one_column_across_producers(#[case] table: &str) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    write_feat_tree(dir.path());
    let db = ingest_with_adapters(dir.path(), &["feat", "fmriprep"]).await?;

    let sql_type: String = db.conn.query_row(
        "SELECT data_type FROM information_schema.columns \
         WHERE table_name = ? AND column_name = 'trans_x'",
        duckdb::params![table],
        |r| r.get(0),
    )?;

    assert_eq!(sql_type, "DOUBLE", "{table}.trans_x");
    Ok(())
}

/// `mc/`, `fix/` and `filtered_func_data.ica/` hold keepers amongst the scratch, so the
/// ignore rules must discriminate within a directory rather than by prefix alone.
#[rstest]
#[case("%/mc/prefiltered_func_data_mcf.par")]
#[case("%/melodic_mix")]
#[case("%/melodic_FTmix")]
#[case("%/melodic_ICstats")]
#[case("%/filtered_func_data.ica/mean.nii.gz")]
#[case("%/fix/features.csv")]
#[tokio::test]
async fn a_keeper_beside_the_scratch_survives(#[case] keeper: &str) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    write_feat_tree(dir.path());
    let db = ingest_with_adapters(dir.path(), &["feat"]).await?;

    let got: i64 = db.conn.query_row(
        "SELECT COUNT(*) FROM all_files WHERE datatype IS NOT NULL AND file_path LIKE ?",
        duckdb::params![keeper],
        |r| r.get(0),
    )?;

    assert_eq!(got, 1, "keeper should survive: {keeper}");
    Ok(())
}
