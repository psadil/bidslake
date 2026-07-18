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
