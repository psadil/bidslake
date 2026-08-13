//! A dataset may span several ingest roots (docs/adr/0005).
//!
//! Subject-sharded pipeline output — the normal way fMRIPrep and FreeSurfer are run at
//! scale — is one logical dataset with one root per subject. These tests cover the rule
//! that decides when two roots are one dataset, which turns on whether the `dataset_id`
//! was asserted by the user or inferred from `dataset_description.json`.

mod common;

use serde_json::json;
use std::path::Path;

/// A minimal shard: one subject's anatomical, plus a `dataset_description.json` built from
/// `desc`. `desc` is what the identity rule compares, so each test varies just the field
/// it is about.
fn shard(dir: &Path, sub: &str, desc: serde_json::Value) -> anyhow::Result<()> {
    let anat = dir.join(format!("sub-{sub}/anat"));
    std::fs::create_dir_all(&anat)?;
    std::fs::write(anat.join(format!("sub-{sub}_T1w.nii.gz")), b"nii")?;
    std::fs::write(
        dir.join("dataset_description.json"),
        serde_json::to_string_pretty(&desc)?,
    )?;
    Ok(())
}

/// What every shard of one fMRIPrep run declares. `Name` is the *tool's* name, which is
/// why it cannot identify a dataset on its own.
fn fmriprep_desc(source_doi: &str) -> serde_json::Value {
    json!({
        "Name": "fMRIPrep - fMRI PREProcessing workflow",
        "BIDSVersion": "1.4.0",
        "DatasetType": "derivative",
        "GeneratedBy": [{"Name": "fMRIPrep", "Version": "20.2.1"}],
        "SourceDatasets": [{"DOI": source_doi}],
    })
}

fn uri_for(dir: &Path) -> anyhow::Result<String> {
    Ok(format!("file://{}", dir.canonicalize()?.display()))
}

/// Two shards of one pipeline run carry byte-identical descriptions, so indexing them with
/// no flags at all merges them: one dataset, two roots, one participants list. This is the
/// headline case — the sharded workflow needs no ceremony.
#[tokio::test]
async fn identical_descriptions_merge_with_no_flags() -> anyhow::Result<()> {
    let (a, b) = (tempfile::tempdir()?, tempfile::tempdir()?);
    shard(a.path(), "01", fmriprep_desc("10.18112/openneuro.ds001.v1"))?;
    shard(b.path(), "02", fmriprep_desc("10.18112/openneuro.ds001.v1"))?;

    let db = bidslake::db::BidsDb::new(":memory:")?;
    common::ingest_inferred_into(&db, a.path()).await?;
    common::ingest_inferred_into(&db, b.path()).await?;

    let id = "fMRIPrep - fMRI PREProcessing workflow";
    let roots = db.dataset_roots(id)?;
    // Sorted on both sides: `dataset_roots` orders by URI, and `tempdir()` names are
    // random, so the two roots come back in either order.
    let mut want = vec![uri_for(a.path())?, uri_for(b.path())?];
    want.sort();
    assert_eq!(roots, want);

    assert_eq!(
        common::count(&db, "dataset_description")?,
        1,
        "two shards are one dataset"
    );
    let subjects: i64 = db.conn.query_row(
        "SELECT COUNT(*) FROM participants WHERE dataset_id = ?",
        [id],
        |r| r.get(0),
    )?;
    assert_eq!(subjects, 2, "participants spans both roots");
    Ok(())
}

/// Two *studies* run through the same pipeline share a `Name` — it is the tool's — but
/// declare different sources. With the id inferred, bidslake cannot tell a shard from an
/// unrelated study on the name alone, so it refuses rather than silently merging them, and
/// names both ways out.
#[tokio::test]
async fn a_different_source_dataset_is_refused_when_the_id_was_inferred() -> anyhow::Result<()> {
    let (a, b) = (tempfile::tempdir()?, tempfile::tempdir()?);
    shard(a.path(), "01", fmriprep_desc("10.18112/openneuro.ds001.v1"))?;
    shard(b.path(), "01", fmriprep_desc("10.18112/openneuro.ds002.v1"))?;

    let db = bidslake::db::BidsDb::new(":memory:")?;
    common::ingest_inferred_into(&db, a.path()).await?;
    let err = common::ingest_inferred_into(&db, b.path())
        .await
        .expect_err("two studies' output must not merge on a shared Name");

    let msg = err.to_string();
    assert!(msg.contains("SourceDatasets"), "{msg}");
    assert!(msg.contains("--dataset-id"), "{msg}");
    // The refusal must leave the first root alone rather than half-registering the second.
    assert_eq!(
        db.dataset_roots("fMRIPrep - fMRI PREProcessing workflow")?,
        vec![uri_for(a.path())?]
    );
    Ok(())
}

/// The same two roots, but the user asserted the id: that is a claim bidslake has no
/// standing to overrule, so they merge. It warns rather than proceeding in silence, since
/// under an asserted id a disagreeing description is the best available evidence of a typo.
#[tokio::test]
async fn an_asserted_dataset_id_merges_differing_descriptions() -> anyhow::Result<()> {
    let (a, b) = (tempfile::tempdir()?, tempfile::tempdir()?);
    shard(a.path(), "01", fmriprep_desc("10.18112/openneuro.ds001.v1"))?;
    shard(b.path(), "02", fmriprep_desc("10.18112/openneuro.ds002.v1"))?;

    let db = common::ingest_as(a.path(), "study").await?;
    common::ingest_into(&db, b.path(), "study").await?;

    assert_eq!(db.dataset_roots("study")?.len(), 2);
    Ok(())
}

/// A differing `GeneratedBy` is enough on its own: two shards processed months apart by
/// different pipeline versions are not obviously one dataset, and inference does not guess.
#[tokio::test]
async fn a_different_pipeline_version_is_refused_when_the_id_was_inferred() -> anyhow::Result<()> {
    let (a, b) = (tempfile::tempdir()?, tempfile::tempdir()?);
    shard(a.path(), "01", fmriprep_desc("10.18112/openneuro.ds001.v1"))?;
    let mut newer = fmriprep_desc("10.18112/openneuro.ds001.v1");
    newer["GeneratedBy"] = json!([{"Name": "fMRIPrep", "Version": "23.1.0"}]);
    shard(b.path(), "02", newer)?;

    let db = bidslake::db::BidsDb::new(":memory:")?;
    common::ingest_inferred_into(&db, a.path()).await?;
    let err = common::ingest_inferred_into(&db, b.path())
        .await
        .expect_err("a different pipeline version must not merge silently");
    assert!(err.to_string().contains("GeneratedBy"), "{err}");
    Ok(())
}

/// Key order in `SourceDatasets` is not a difference. The stored side is compact JSON text
/// and the incoming side is a parsed value, so this only holds because the comparison parses
/// before comparing — a textual one would report a spurious mismatch and refuse a valid merge.
#[tokio::test]
async fn reordered_description_keys_still_merge() -> anyhow::Result<()> {
    let (a, b) = (tempfile::tempdir()?, tempfile::tempdir()?);
    shard(
        a.path(),
        "01",
        json!({
            "Name": "study", "BIDSVersion": "1.8.0",
            "SourceDatasets": [{"DOI": "10.0/x", "URL": "https://example.org/x"}],
        }),
    )?;
    shard(
        b.path(),
        "02",
        json!({
            "BIDSVersion": "1.8.0", "Name": "study",
            "SourceDatasets": [{"URL": "https://example.org/x", "DOI": "10.0/x"}],
        }),
    )?;

    let db = bidslake::db::BidsDb::new(":memory:")?;
    common::ingest_inferred_into(&db, a.path()).await?;
    common::ingest_inferred_into(&db, b.path()).await?;
    assert_eq!(db.dataset_roots("study")?.len(), 2);
    Ok(())
}

/// Re-indexing a root already registered is a no-op on `dataset_roots`, not a second row
/// and not a primary-key violation.
#[tokio::test]
async fn reindexing_a_registered_root_adds_nothing() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    shard(
        dir.path(),
        "01",
        fmriprep_desc("10.18112/openneuro.ds001.v1"),
    )?;

    let db = bidslake::db::BidsDb::new(":memory:")?;
    common::ingest_inferred_into(&db, dir.path()).await?;
    common::ingest_inferred_into(&db, dir.path()).await?;

    assert_eq!(common::count(&db, "dataset_roots")?, 1);
    Ok(())
}

/// A dataset with no `dataset_description.json` — a FreeSurfer `recon-all` tree, the other
/// sharded pipeline — infers its id from the *root's* directory name. Running `recon-all`
/// one subject at a time gives one `SUBJECTS_DIR` per subject, and those directories
/// conventionally share a name (`.../sub-01/freesurfer`, `.../sub-02/freesurfer`), so
/// inference lands on one id and the shards merge with no flags. Nothing about the two
/// descriptions can disagree, because neither exists.
///
/// Note the root is the `SUBJECTS_DIR` — the directory holding `sub-01/`, `sub-02/` — not a
/// subject directory: every mapping in the bundled term map is anchored at the subject dir
/// relative to the root, so descending one level further projects nothing (ADR 0002 §3).
#[tokio::test]
async fn description_less_roots_sharing_a_directory_name_merge() -> anyhow::Result<()> {
    let parent = tempfile::tempdir()?;
    let (a, b) = (
        parent.path().join("sub-01/freesurfer"),
        parent.path().join("sub-02/freesurfer"),
    );
    for (subjects_dir, sub) in [(&a, "01"), (&b, "02")] {
        let stats = subjects_dir.join(format!("sub-{sub}/anat"));
        std::fs::create_dir_all(&stats)?;
        std::fs::write(stats.join(format!("sub-{sub}_T1w.nii.gz")), b"nii")?;
    }

    let db = bidslake::db::BidsDb::new(":memory:")?;
    common::ingest_inferred_into(&db, &a).await?;
    common::ingest_inferred_into(&db, &b).await?;

    assert_eq!(common::count(&db, "dataset_description")?, 1);
    assert_eq!(db.dataset_roots("freesurfer")?.len(), 2);
    let subjects: i64 = db.conn.query_row(
        "SELECT COUNT(*) FROM participants WHERE dataset_id = 'freesurfer'",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(subjects, 2, "participants spans both SUBJECTS_DIRs");
    Ok(())
}

/// The limitation that remains, stated as what it actually is: it is the *root directory
/// name* that inference keys on, so roots named after their subject
/// (`.../fs/sub-01`, `.../fs/sub-02`) infer different ids and stay separate datasets. That
/// is right by default — nothing distinguishes them from two unrelated trees — and naming
/// `--dataset-id` is what joins them.
#[tokio::test]
async fn roots_named_per_subject_need_an_explicit_dataset_id() -> anyhow::Result<()> {
    let parent = tempfile::tempdir()?;
    let (a, b) = (parent.path().join("sub-01"), parent.path().join("sub-02"));
    for (dir, sub) in [(&a, "01"), (&b, "02")] {
        let anat = dir.join(format!("sub-{sub}/anat"));
        std::fs::create_dir_all(&anat)?;
        std::fs::write(anat.join(format!("sub-{sub}_T1w.nii.gz")), b"nii")?;
    }

    let db = bidslake::db::BidsDb::new(":memory:")?;
    common::ingest_inferred_into(&db, &a).await?;
    common::ingest_inferred_into(&db, &b).await?;
    assert_eq!(common::count(&db, "dataset_description")?, 2);

    let db = bidslake::db::BidsDb::new(":memory:")?;
    common::ingest_into(&db, &a, "freesurfer").await?;
    common::ingest_into(&db, &b, "freesurfer").await?;
    assert_eq!(db.dataset_roots("freesurfer")?.len(), 2);
    Ok(())
}
