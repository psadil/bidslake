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

use common::ingest_with_adapters;
use rstest::rstest;
use std::fs;
use std::path::Path;

fn write(root: &Path, rel: &str, content: &[u8]) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

const UNIT: &str = "sub-01_ses-V1_task-rest_run-01_desc-preproc_bold";
/// A second unit, sessionless and runless, to pin the optional groups in the templates.
const BARE: &str = "sub-02_task-cuff_desc-preproc_bold";

fn write_feat_tree(root: &Path) {
    for (rel, body) in [
        // -- the outputs that matter -------------------------------------------------
        ("filtered_func_data.nii.gz", &b"nii"[..]),
        ("filtered_func_data_clean.nii.gz", b"nii"),
        ("filtered_func_data_clean_vn.nii.gz", b"nii"),
        ("mask.nii.gz", b"nii"),
        ("filtered_func_data.ica/melodic_mix", b"1 2\n3 4\n"),
        ("filtered_func_data.ica/melodic_IC.nii.gz", b"nii"),
        ("filtered_func_data.ica/melodic_oIC.nii.gz", b"nii"),
        ("mc/prefiltered_func_data_mcf.par", b"0 0 0 0 0 0\n"),
        ("fix4melview_UKBiobank_thr1.txt", b"1, Signal\n"),
        ("fix4melview_UKBiobank_thr1_psadil.txt", b"1, Signal\n"),
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
        ("fix/features.csv", b"a,b\n"),
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

/// Each FEAT slot is reachable by what it *is*, not by where it sits.
#[rstest]
#[case("bold", "clean", 1)]
#[case("bold", "filtered", 1)]
#[case("bold", "cleanvn", 1)]
// The mask is the one slot both units write, so it is the only count above one.
#[case("mask", "brain", 2)]
#[case("mixing", "MELODIC", 1)]
#[case("timeseries", "motion", 1)]
#[case("components", "IC", 1)]
#[case("components", "oIC", 1)]
#[case("boldref", "preproc", 1)]
#[case("dseg", "pveseg", 1)]
#[case("T1w", "standard", 1)]
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
        "SELECT COUNT(*) FROM all_files WHERE kind = 'data' AND suffix = ? AND \"desc\" = ?",
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
        "SELECT sub, ses, task, run FROM all_files WHERE kind = 'data' AND suffix = 'mixing'",
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
        "SELECT COUNT(*) FROM all_files WHERE kind = 'data' AND sub = '02' AND ses IS NULL AND run IS NULL \
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
        "SELECT COUNT(*) FROM all_files WHERE kind = 'data' AND suffix = 'xfm' \
         AND \"from\" = ? AND \"to\" = ? AND extension = ? AND mode = 'image'",
        duckdb::params![from, to, ext],
        |r| r.get(0),
    )?;

    assert_eq!(got, 1, "{from} -> {to} ({ext})");
    Ok(())
}

/// A hand-edited classification is the scientifically valuable artifact in this tree, and
/// it differs from the automatic one only by a trailing `_<rater>` — so `rater` is what
/// separates them, and its absence is what marks a unit as not yet reviewed.
#[tokio::test]
async fn rater_separates_hand_classification_from_automatic() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    write_feat_tree(dir.path());
    let db = ingest_with_adapters(dir.path(), &["feat"]).await?;

    let automatic: i64 = db.conn.query_row(
        "SELECT COUNT(*) FROM all_files WHERE kind = 'data' AND suffix = 'classification' AND rater IS NULL",
        [],
        |r| r.get(0),
    )?;
    let by_hand: i64 = db.conn.query_row(
        "SELECT COUNT(*) FROM all_files WHERE kind = 'data' AND suffix = 'classification' AND rater = 'psadil'",
        [],
        |r| r.get(0),
    )?;
    assert_eq!((automatic, by_hand), (1, 1));

    // The training set the classifier used lands in `desc`, so runs classified with
    // different training data stay distinguishable.
    let desc: String = db.conn.query_row(
        "SELECT DISTINCT \"desc\" FROM all_files WHERE kind = 'data' AND suffix = 'classification'",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(desc, "UKBiobank");
    Ok(())
}

/// Most of a FEAT tree is intermediates. They must be recognized (so they are not treated
/// as stray BIDS files) and then dropped — otherwise a single run contributes hundreds of
/// meaningless registry rows.
#[rstest]
#[case("%/fix/%")]
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
        "SELECT COUNT(*) FROM all_files WHERE kind = 'data' AND file_path LIKE ?",
        duckdb::params![pattern],
        |r| r.get(0),
    )?;

    assert_eq!(got, 0, "scratch should not be cataloged: {pattern}");
    Ok(())
}

/// `mc/` and `filtered_func_data.ica/` hold one keeper each amongst the scratch, so the
/// ignore rules must discriminate within a directory rather than by prefix alone.
#[rstest]
#[case("%/mc/prefiltered_func_data_mcf.par")]
#[case("%/melodic_mix")]
#[tokio::test]
async fn a_keeper_beside_the_scratch_survives(#[case] keeper: &str) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    write_feat_tree(dir.path());
    let db = ingest_with_adapters(dir.path(), &["feat"]).await?;

    let got: i64 = db.conn.query_row(
        "SELECT COUNT(*) FROM all_files WHERE kind = 'data' AND file_path LIKE ?",
        duckdb::params![keeper],
        |r| r.get(0),
    )?;

    assert_eq!(got, 1, "keeper should survive: {keeper}");
    Ok(())
}
