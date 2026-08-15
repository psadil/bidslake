//! Cross-dataset association (docs/adr/0003): datasets that declare the same `SourceDatasets`
//! are co-derivatives (`shares_source`), resolved at query time so ingest order is irrelevant.
//!
//! The load-bearing tests: `shares_source_by_shared_doi` (the bare-DOI/URL-DOI normalization the
//! whole feature turns on), `shares_source_without_source_in_catalog` (the shared source need not
//! be present), and `unrelated_datasets_have_no_relation` (matching `sub-01` with no shared source
//! must produce **zero** edges — proof we did not re-institutionalize the unsound cross-dataset
//! entity join).

use bidslake::bids::BidsParser;
use bidslake::db::BidsDb;
use bidslake::fs::LocalFileSystem;
use bidslake::schema::Schema;
use std::fs;
use std::path::Path;

/// Write a minimal derivative dataset: a `dataset_description.json` declaring the given
/// `SourceDatasets` (each a `{"DOI": …}` entry), an optional `DatasetDOI`, and one data file.
fn write_dataset(root: &Path, name: &str, sources: &[&str], dataset_doi: Option<&str>) {
    fs::create_dir_all(root.join("sub-01/anat")).unwrap();
    let mut desc = serde_json::json!({
        "Name": name, "BIDSVersion": "1.9.0", "DatasetType": "derivative",
    });
    if !sources.is_empty() {
        desc["SourceDatasets"] = sources
            .iter()
            .map(|s| serde_json::json!({ "DOI": s }))
            .collect();
    }
    if let Some(doi) = dataset_doi {
        desc["DatasetDOI"] = serde_json::json!(doi);
    }
    fs::write(
        root.join("dataset_description.json"),
        serde_json::to_string(&desc).unwrap(),
    )
    .unwrap();
    fs::write(root.join("sub-01/anat/sub-01_T1w.nii.gz"), b"").unwrap();
}

fn empty_db() -> BidsDb {
    let db = BidsDb::new(":memory:").unwrap();
    db.create_tables(&Schema::load(None).unwrap()).unwrap();
    db
}

/// Ingest a tree into an existing catalog under `dataset_id`, with optional `--source-dataset` refs.
async fn ingest_into(db: &BidsDb, path: &Path, dataset_id: &str, declared: &[&str]) {
    let fs = Box::new(LocalFileSystem::new(path.to_path_buf()));
    let mut parser = BidsParser::new(
        fs,
        Some(dataset_id.to_string()),
        Schema::load(None).unwrap(),
        None,
        true,
        true,
    )
    .with_declared_sources(declared.iter().map(|s| s.to_string()).collect());
    let txn = db.conn.unchecked_transaction().unwrap();
    parser.parse(db).await.unwrap();
    txn.commit().unwrap();
}

/// All `(from, to, relation)` edges, sorted deterministically.
fn relations(db: &BidsDb) -> Vec<(String, String, String)> {
    let mut stmt = db
        .conn
        .prepare(
            "SELECT from_dataset_id, to_dataset_id, relation FROM dataset_relations \
             ORDER BY from_dataset_id, to_dataset_id, relation",
        )
        .unwrap();
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn count(db: &BidsDb, sql: &str) -> i64 {
    db.conn.query_row(sql, [], |r| r.get(0)).unwrap()
}

/// Write a dataset whose `dataset_description.json` carries a BIDS `DatasetLinks` map —
/// the *naming* half of `dataset_links` (`link_type='named'`).
fn write_dataset_with_links(root: &Path, name: &str, links: &[(&str, &str)]) {
    write_dataset(root, name, &[], None);
    let path = root.join("dataset_description.json");
    let mut desc: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    desc["DatasetLinks"] = links
        .iter()
        .map(|(k, v)| (k.to_string(), serde_json::json!(v)))
        .collect::<serde_json::Map<_, _>>()
        .into();
    fs::write(&path, serde_json::to_string(&desc).unwrap()).unwrap();
}

/// All `(from, link_name, target)` rows of the naming resolver, sorted. `target` is `None`
/// when nothing in the catalog holds the identity yet.
fn link_targets(db: &BidsDb) -> Vec<(String, String, Option<String>)> {
    let mut stmt = db
        .conn
        .prepare(
            "SELECT from_dataset_id, link_name, target_dataset_id FROM dataset_link_targets \
             ORDER BY from_dataset_id, link_name",
        )
        .unwrap();
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

#[tokio::test]
async fn shares_source_by_shared_doi() -> anyhow::Result<()> {
    // fMRIPrep declares the DOI as a URL, MRIQC as the bare DOI — normalization must collide them.
    let tmp = tempfile::tempdir()?;
    let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
    write_dataset(
        &a,
        "fmriprep",
        &["https://doi.org/10.18112/openneuro.ds001761.v2.0.1"],
        None,
    );
    write_dataset(&b, "mriqc", &["10.18112/openneuro.ds001761.v2.0.1"], None);
    let db = empty_db();
    ingest_into(&db, &a, "fmriprep", &[]).await;
    ingest_into(&db, &b, "mriqc", &[]).await;
    assert_eq!(
        relations(&db),
        vec![
            ("fmriprep".into(), "mriqc".into(), "shares_source".into()),
            ("mriqc".into(), "fmriprep".into(), "shares_source".into()),
        ]
    );
    Ok(())
}

#[tokio::test]
async fn shares_source_without_source_in_catalog() -> anyhow::Result<()> {
    // The shared source (the raw dataset) is never ingested — only the two derivatives are.
    let tmp = tempfile::tempdir()?;
    let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
    write_dataset(
        &a,
        "fmriprep",
        &["10.18112/openneuro.ds001761.v2.0.1"],
        None,
    );
    write_dataset(&b, "mriqc", &["10.18112/openneuro.ds001761.v2.0.1"], None);
    let db = empty_db();
    ingest_into(&db, &a, "fmriprep", &[]).await;
    ingest_into(&db, &b, "mriqc", &[]).await;
    assert_eq!(
        count(&db, "SELECT COUNT(*) FROM dataset_description"),
        2,
        "raw source not present"
    );
    assert!(
        !relations(&db).is_empty(),
        "the edge resolves without the source in the catalog"
    );
    Ok(())
}

/// The query-layer payoff of multi-root datasets (docs/adr/0005).
///
/// Shards of one pipeline run all declare the same `SourceDatasets`, so when each was its
/// own `dataset_id` the `shares_source` view fired **between them** — N×(N−1) edges of a
/// dataset with itself, burying the one relation a consumer wants. Sharing a `dataset_id`
/// removes them at the source: the view's `from <> to` drops the self-pairs, and the real
/// fMRIPrep↔MRIQC edge is all that is left.
#[tokio::test]
async fn shards_of_one_dataset_do_not_relate_to_each_other() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let doi = "10.18112/openneuro.ds001761.v2.0.1";
    let (s1, s2, qc) = (
        tmp.path().join("fmriprep-sub-01"),
        tmp.path().join("fmriprep-sub-02"),
        tmp.path().join("mriqc"),
    );
    // Two fMRIPrep shards — identical descriptions, as one pipeline run produces — plus an
    // unrelated-by-tool but same-source MRIQC derivative.
    write_dataset(&s1, "fMRIPrep - fMRI PREProcessing workflow", &[doi], None);
    write_dataset(&s2, "fMRIPrep - fMRI PREProcessing workflow", &[doi], None);
    write_dataset(&qc, "MRIQC", &[doi], None);

    let db = empty_db();
    ingest_into(&db, &s1, "fmriprep", &[]).await;
    ingest_into(&db, &s2, "fmriprep", &[]).await;
    ingest_into(&db, &qc, "mriqc", &[]).await;

    assert_eq!(db.dataset_roots("fmriprep")?.len(), 2, "two shards, one id");
    assert_eq!(
        relations(&db),
        vec![
            ("fmriprep".into(), "mriqc".into(), "shares_source".into()),
            ("mriqc".into(), "fmriprep".into(), "shares_source".into()),
        ],
        "only the real cross-pipeline relation survives"
    );
    Ok(())
}

/// Every root of a dataset is an identity it *is*, and re-indexing one must not drop the
/// others. `clear_derived_links` wipes all of a dataset's identities before they are
/// re-recorded, so this only holds because `record_links` re-reads `dataset_roots` rather
/// than recording the single root the current run happens to be walking.
#[tokio::test]
async fn every_root_is_an_identity_and_survives_reindexing_another() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let (a, b) = (tmp.path().join("shard-a"), tmp.path().join("shard-b"));
    write_dataset(&a, "study", &["10.18112/x/y"], None);
    write_dataset(&b, "study", &["10.18112/x/y"], None);

    let db = empty_db();
    ingest_into(&db, &a, "study", &[]).await;
    ingest_into(&db, &b, "study", &[]).await;
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*) FROM dataset_identity WHERE source = 'root_uri'"
        ),
        2,
        "one root_uri identity per root"
    );

    // Re-index the first shard: the second's identity must still be there.
    ingest_into(&db, &a, "study", &[]).await;
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*) FROM dataset_identity WHERE source = 'root_uri'"
        ),
        2,
        "re-indexing one root kept the other's identity"
    );
    Ok(())
}

/// A dataset's description is re-read every run, so a re-index must *refresh* the stored
/// row rather than defer to whatever wrote it first (the `eh-04` follow-up). Before this,
/// a `dataset_description.json` corrected after the first index never reached the catalog.
#[tokio::test]
async fn reindexing_refreshes_the_stored_description() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let root = tmp.path().join("study");
    write_dataset(&root, "study", &["10.18112/x/y"], None);

    let db = empty_db();
    ingest_into(&db, &root, "study", &[]).await;
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*) FROM dataset_description WHERE \"DatasetDOI\" IS NULL"
        ),
        1
    );

    // The description gains a DOI, as one does when a dataset is published.
    write_dataset(&root, "study renamed", &["10.18112/x/y"], Some("10.0/pub"));
    ingest_into(&db, &root, "study", &[]).await;

    let (name, doi): (String, String) = db.conn.query_row(
        "SELECT \"Name\", \"DatasetDOI\" FROM dataset_description WHERE dataset_id = 'study'",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    assert_eq!(name, "study renamed");
    assert_eq!(doi, "10.0/pub");
    assert_eq!(
        count(&db, "SELECT COUNT(*) FROM dataset_description"),
        1,
        "refreshed, not duplicated"
    );
    Ok(())
}

#[tokio::test]
async fn ingest_order_does_not_matter() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
    write_dataset(&a, "a", &["10.18112/x/y"], None);
    write_dataset(&b, "b", &["10.18112/x/y"], None);

    let db1 = empty_db();
    ingest_into(&db1, &a, "a", &[]).await;
    ingest_into(&db1, &b, "b", &[]).await;

    let db2 = empty_db();
    ingest_into(&db2, &b, "b", &[]).await;
    ingest_into(&db2, &a, "a", &[]).await;

    assert_eq!(relations(&db1), relations(&db2));
    assert!(!relations(&db1).is_empty());
    Ok(())
}

#[tokio::test]
async fn derived_from_when_source_present() -> anyhow::Result<()> {
    // A raw dataset that *is* the DOI, and a derivative that declares it as a source.
    let tmp = tempfile::tempdir()?;
    let (raw, deriv) = (tmp.path().join("raw"), tmp.path().join("deriv"));
    write_dataset(&raw, "raw", &[], Some("10.18112/x/y"));
    write_dataset(&deriv, "deriv", &["10.18112/x/y"], None);
    let db = empty_db();
    ingest_into(&db, &raw, "raw", &[]).await;
    ingest_into(&db, &deriv, "deriv", &[]).await;
    let rels = relations(&db);
    assert!(rels.contains(&("deriv".into(), "raw".into(), "derived_from".into())));
    assert!(rels.contains(&("raw".into(), "deriv".into(), "source_of".into())));
    Ok(())
}

#[tokio::test]
async fn unrelated_datasets_have_no_relation() -> anyhow::Result<()> {
    // Both have sub-01, but they share no source — there must be NO edge.
    let tmp = tempfile::tempdir()?;
    let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
    write_dataset(&a, "a", &["10.18112/aaa/1"], None);
    write_dataset(&b, "b", &["10.18112/bbb/2"], None);
    let db = empty_db();
    ingest_into(&db, &a, "a", &[]).await;
    ingest_into(&db, &b, "b", &[]).await;
    assert!(
        relations(&db).is_empty(),
        "unrelated datasets must not relate"
    );
    Ok(())
}

#[tokio::test]
async fn declared_source_dataset_links() -> anyhow::Result<()> {
    // The escape hatch: `--source-dataset <bare id>` → a derived_from edge against `self`.
    let tmp = tempfile::tempdir()?;
    let (base, deriv) = (tmp.path().join("base"), tmp.path().join("deriv"));
    write_dataset(&base, "base", &[], None);
    write_dataset(&deriv, "deriv", &[], None); // no DOI at all
    let db = empty_db();
    ingest_into(&db, &base, "base", &[]).await;
    ingest_into(&db, &deriv, "deriv", &["base"]).await;
    assert!(relations(&db).contains(&("deriv".into(), "base".into(), "derived_from".into())));
    Ok(())
}

#[tokio::test]
async fn nested_dataset_description_does_not_declare_links() -> anyhow::Result<()> {
    // A raw dataset containing derivatives/x/dataset_description.json with a DOI must not record
    // that DOI under the PARENT's dataset_id.
    let tmp = tempfile::tempdir()?;
    let root = tmp.path().join("root");
    write_dataset(&root, "root", &[], None); // the root declares no source
    let nested = root.join("derivatives/x");
    fs::create_dir_all(&nested).unwrap();
    fs::write(
        nested.join("dataset_description.json"),
        r#"{"Name":"x","BIDSVersion":"1.9.0","DatasetType":"derivative","SourceDatasets":[{"DOI":"10.18112/nested/1"}]}"#,
    )
    .unwrap();
    let db = empty_db();
    ingest_into(&db, &root, "root", &[]).await;
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*) FROM dataset_links WHERE dataset_id='root' AND link_type='source'"
        ),
        0,
        "the nested description's SourceDatasets belongs to the nested dataset, not the root",
    );
    Ok(())
}

#[tokio::test]
async fn reingest_refreshes_declarations() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let a = tmp.path().join("a");
    write_dataset(&a, "a", &["10.18112/x/y"], None);
    let db = empty_db();
    ingest_into(&db, &a, "a", &[]).await;
    ingest_into(&db, &a, "a", &[]).await; // idempotent — must not duplicate or error
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*) FROM dataset_links WHERE dataset_id='a'"
        ),
        1,
    );
    Ok(())
}

#[tokio::test]
async fn unparseable_source_is_opaque_not_dropped() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let a = tmp.path().join("a");
    write_dataset(&a, "a", &["some free text source"], None);
    let db = empty_db();
    ingest_into(&db, &a, "a", &[]).await;
    let kind: String = db.conn.query_row(
        "SELECT identity_kind FROM dataset_links WHERE dataset_id='a' AND link_type='source'",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(kind, "opaque");
    Ok(())
}

// ---------------------------------------------------------------------------
// Naming links (`named`/`alias`) — the other half of `dataset_links`.
//
// `dataset_links` holds two kinds of statement: provenance ("came from") and naming
// ("here, N refers to L"). Only provenance may reach `dataset_relations`; naming resolves
// through `dataset_link_targets`. Keeping them apart is what lets a query name a dataset
// without hardcoding an id, and is why these tests assert an *absence* as hard as a
// presence.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_named_link_is_not_a_derivation() -> anyhow::Result<()> {
    // studyA merely *references* studyB. Referencing is not deriving: a study pointing at a
    // template, an atlas or a shared recon-all tree did not come from it. This regressed a
    // real bug — the `derived_from`/`source_of` arms filtered no `link_type` at all, so every
    // `DatasetLinks` entry read as a derivation.
    let tmp = tempfile::tempdir()?;
    let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
    write_dataset_with_links(&a, "studyA", &[("fs", "dataset:studyB")]);
    write_dataset(&b, "studyB", &[], None);
    let db = empty_db();
    ingest_into(&db, &b, "studyB", &[]).await;
    ingest_into(&db, &a, "studyA", &[]).await;

    assert_eq!(
        count(
            &db,
            "SELECT count(*) FROM dataset_links WHERE dataset_id='studyA' AND link_type='named'"
        ),
        1,
        "the DatasetLinks entry must still be stored"
    );
    assert!(
        relations(&db).is_empty(),
        "a named link must produce no relation, got {:?}",
        relations(&db)
    );
    assert_eq!(
        link_targets(&db),
        vec![(
            "studyA".to_string(),
            "fs".to_string(),
            Some("studyB".to_string())
        )],
        "it must resolve as a *name* instead"
    );
    Ok(())
}

#[tokio::test]
async fn an_alias_survives_reindexing_and_makes_no_relation() -> anyhow::Result<()> {
    // `alias` is the user-asserted counterpart of `DatasetLinks`, so it follows `declared`:
    // never cleared by a re-ingest. A `named` link in its place would be wiped, which is the
    // whole reason a separate link_type exists.
    let tmp = tempfile::tempdir()?;
    let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
    write_dataset(&a, "studyA", &[], None);
    write_dataset(&b, "studyB", &[], None);
    let db = empty_db();
    ingest_into(&db, &a, "studyA", &[]).await;
    ingest_into(&db, &b, "studyB", &[]).await;

    db.record_dataset_link(
        "studyA",
        "alias",
        "fs",
        "dataset:studyB",
        &bidslake::links::canonicalize("dataset:studyB"),
    )?;
    assert_eq!(
        link_targets(&db),
        vec![(
            "studyA".to_string(),
            "fs".to_string(),
            Some("studyB".to_string())
        )]
    );

    ingest_into(&db, &a, "studyA", &[]).await; // re-index the dataset the alias lives in
    assert_eq!(
        link_targets(&db),
        vec![(
            "studyA".to_string(),
            "fs".to_string(),
            Some("studyB".to_string())
        )],
        "an alias must survive a re-index"
    );
    assert!(
        relations(&db).is_empty(),
        "an alias must produce no relation, got {:?}",
        relations(&db)
    );
    Ok(())
}

#[tokio::test]
async fn an_alias_may_name_a_target_indexed_later() -> anyhow::Result<()> {
    // The property the view buys, and the reason the target is not stored: naming a dataset
    // that is not in the catalog yet is a legitimate forward reference, not an error. It
    // starts resolving when the target arrives — no re-index of the naming dataset, no
    // re-running of `link alias`. Same argument as ADR 0003 §2's `ingest_order_does_not_matter`.
    let tmp = tempfile::tempdir()?;
    let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
    write_dataset(&a, "studyA", &[], None);
    write_dataset(&b, "studyB", &[], None);
    let db = empty_db();
    ingest_into(&db, &a, "studyA", &[]).await;
    db.record_dataset_link(
        "studyA",
        "alias",
        "fs",
        "dataset:studyB",
        &bidslake::links::canonicalize("dataset:studyB"),
    )?;

    assert_eq!(
        link_targets(&db),
        vec![("studyA".to_string(), "fs".to_string(), None)],
        "the row exists but resolves to nothing — which is how a caller tells \
         'not indexed yet' from 'misspelled name'"
    );

    ingest_into(&db, &b, "studyB", &[]).await;
    assert_eq!(
        link_targets(&db),
        vec![(
            "studyA".to_string(),
            "fs".to_string(),
            Some("studyB".to_string())
        )],
        "and resolves once the target is indexed, with nothing else re-run"
    );
    Ok(())
}

#[tokio::test]
async fn a_root_relative_dataset_link_resolves_to_its_target() -> anyhow::Result<()> {
    // The form BIDS actually writes. A `DatasetLinks` value is relative to the dataset root,
    // so it only becomes an identity the target can hold once it is joined to that root —
    // before that it canonicalized to `dataset:../freesurfer`, which nothing is.
    let tmp = tempfile::tempdir()?;
    let study = tmp.path().join("study");
    let (deriv, fs_tree) = (study.join("fmriprep"), study.join("freesurfer"));
    write_dataset_with_links(&deriv, "fmriprep", &[("fs", "../freesurfer")]);
    write_dataset(&fs_tree, "freesurfer", &[], None);

    let db = empty_db();
    ingest_into(&db, &deriv, "fmriprep", &[]).await;
    ingest_into(&db, &fs_tree, "freesurfer", &[]).await;

    assert_eq!(
        link_targets(&db),
        vec![(
            "fmriprep".to_string(),
            "fs".to_string(),
            Some("freesurfer".to_string())
        )]
    );
    assert!(
        relations(&db).is_empty(),
        "and it is still not a derivation, got {:?}",
        relations(&db)
    );
    Ok(())
}
