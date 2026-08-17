//! Root tenure: what was promised about a root, and so what the catalog may conclude
//! (docs/adr/0007).
//!
//! The load-bearing property, and the one easy to regress: `--managed` is an assertion that a
//! later plain re-index must not silently withdraw, since demoting a root quietly revokes the
//! authority its rows carry.

mod common;

use bidslake::db::BidsDb;
use bidslake::fs::{local_root, root_uri};
use bidslake::schema::Tenure;
use std::path::Path;

// -- addressing a root -------------------------------------------------------------------

#[test]
fn the_uri_conversion_round_trips() -> anyhow::Result<()> {
    // These two are inverses, and consumers rely on that to turn an output directory into
    // something a `root_uri = ?` predicate can match.
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).canonicalize()?;
    assert_eq!(
        local_root(&root_uri(&path)).as_deref(),
        Some(path.as_path())
    );
    Ok(())
}

// -- tenure on the registry --------------------------------------------------------------

fn tenure_of(db: &BidsDb, id: &str, uri: &str) -> anyhow::Result<Option<Tenure>> {
    db.dataset_root_tenure(id, uri)
}

#[tokio::test]
async fn indexing_records_attached_by_default() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let anat = dir.path().join("sub-01/anat");
    std::fs::create_dir_all(&anat)?;
    std::fs::write(anat.join("sub-01_T1w.nii.gz"), b"nii")?;

    let db = BidsDb::new(":memory:")?;
    common::ingest_into(&db, dir.path(), "study").await?;

    let uri = root_uri(&dir.path().canonicalize()?);
    assert_eq!(tenure_of(&db, "study", &uri)?, Some(Tenure::Attached));
    Ok(())
}

#[test]
fn a_plain_reindex_does_not_demote_a_managed_root() -> anyhow::Result<()> {
    let db = BidsDb::new(":memory:")?;
    db.create_tables(&bidslake::schema::Schema::load(None).unwrap())?;

    db.register_dataset_root("study", "file:///data/study", Tenure::Managed)?;
    // The run that follows says nothing about tenure — which is not the same as saying
    // "attached". Omission is not retraction.
    db.register_dataset_root("study", "file:///data/study", Tenure::Attached)?;

    assert_eq!(
        tenure_of(&db, "study", "file:///data/study")?,
        Some(Tenure::Managed)
    );
    // And still one root, not two (the invariant test_dataset_roots.rs already pins).
    assert_eq!(db.dataset_roots("study")?.len(), 1);
    Ok(())
}

#[test]
fn managed_can_be_asserted_over_an_existing_attached_root() -> anyhow::Result<()> {
    let db = BidsDb::new(":memory:")?;
    db.create_tables(&bidslake::schema::Schema::load(None).unwrap())?;

    db.register_dataset_root("study", "file:///data/study", Tenure::Attached)?;
    // Bringing an already-indexed tree under management is the whole transition the ADR
    // describes, so it has to work without a re-index from scratch.
    db.register_dataset_root("study", "file:///data/study", Tenure::Managed)?;

    assert_eq!(
        tenure_of(&db, "study", "file:///data/study")?,
        Some(Tenure::Managed)
    );
    assert_eq!(db.dataset_roots("study")?.len(), 1);
    Ok(())
}

#[test]
fn a_catalog_built_before_tenure_gains_the_column() -> anyhow::Result<()> {
    let db = BidsDb::new(":memory:")?;
    // A pre-ADR-0009 `dataset_roots`, which `CREATE TABLE IF NOT EXISTS` would leave alone —
    // so without the ALTER every read of `tenure` fails on an existing catalog.
    db.conn.execute(
        "CREATE TABLE dataset_roots (dataset_id TEXT, root_uri TEXT, \
         PRIMARY KEY (dataset_id, root_uri))",
        [],
    )?;
    db.conn.execute(
        "INSERT INTO dataset_roots VALUES ('study', 'file:///data/study')",
        [],
    )?;

    db.create_tables(&bidslake::schema::Schema::load(None).unwrap())?;

    // Backfilled as `attached`, which is what that root actually promised: it was indexed,
    // so its files were there, and nothing more was claimed.
    assert_eq!(
        tenure_of(&db, "study", "file:///data/study")?,
        Some(Tenure::Attached)
    );
    // And running it again is a no-op rather than an error.
    db.create_tables(&bidslake::schema::Schema::load(None).unwrap())?;
    assert_eq!(db.dataset_roots("study")?.len(), 1);
    Ok(())
}

#[test]
fn reading_an_older_catalog_does_not_need_the_column() -> anyhow::Result<()> {
    // The upgrade in `create_tables` only runs under `index`. Every *read* command opens with
    // `BidsDb::new` and runs no DDL, and the Python bindings open read-only and cannot, so a
    // read that assumed the current shape would fail on any catalog built before this build —
    // with a `Binder Error`, against a user whose only fault was not re-indexing.
    let db = BidsDb::new(":memory:")?;
    db.conn.execute(
        "CREATE TABLE dataset_roots (dataset_id TEXT, root_uri TEXT, \
         PRIMARY KEY (dataset_id, root_uri))",
        [],
    )?;
    db.conn.execute(
        "INSERT INTO dataset_roots VALUES ('study', 'file:///data/study')",
        [],
    )?;

    assert!(!db.has_column("dataset_roots", "tenure")?);
    // Reported as what such a root actually promised, matching the ALTER's backfill default.
    assert_eq!(
        tenure_of(&db, "study", "file:///data/study")?,
        Some(Tenure::Attached)
    );
    assert_eq!(tenure_of(&db, "study", "file:///elsewhere")?, None);
    Ok(())
}

#[test]
fn an_unregistered_root_has_no_tenure() -> anyhow::Result<()> {
    let db = BidsDb::new(":memory:")?;
    db.create_tables(&bidslake::schema::Schema::load(None).unwrap())?;
    // `None` rather than a defaulted `Attached`: "never indexed" and "indexed, promising
    // only durability" are different answers, and conflating them is how a query about a
    // tree nobody ever indexed comes back looking authoritative.
    assert_eq!(tenure_of(&db, "study", "file:///nowhere")?, None);
    Ok(())
}
