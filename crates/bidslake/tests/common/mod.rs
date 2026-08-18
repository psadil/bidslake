//! Shared helpers for integration tests.
//!
//! Included via `mod common;` in each test file. Living under `tests/common/`
//! (a subdirectory) keeps Cargo from compiling it as its own test binary.
//!
//! Not every test binary uses every helper, so suppress the cross-binary
//! "never used" warnings that Cargo's per-binary compilation produces.
#![allow(dead_code)]

use anyhow::Result;
use bids_schema::term_map::{TermMap, bundled_term_map};
use bidslake::schema::{AppliedOverlay, Ingestion};
use bidslake::{bids::BidsParser, db::BidsDb, fs::LocalFileSystem, schema::Schema};
use std::path::{Path, PathBuf};

/// Ingest a BIDS dataset from `dataset_path` into a fresh in-memory DuckDB and
/// return the connection. Using `:memory:` avoids temp-file lifetime juggling
/// and keeps each test fully isolated.
///
/// The whole parse runs inside one transaction, exactly as `main.rs` does — so
/// tests exercise the production path, including its failure mode where a single
/// erroring statement poisons the transaction for the rest of the ingest.
pub async fn ingest(dataset_path: impl AsRef<Path>) -> Result<BidsDb> {
    let db = BidsDb::new(":memory:")?;
    let schema = Schema::load(None).unwrap();
    db.create_tables(&schema)?;

    let fs = Box::new(LocalFileSystem::new(dataset_path.as_ref().to_path_buf()));
    let mut parser = BidsParser::new(fs, None, schema, None, true, true);
    let txn = db.conn.unchecked_transaction()?;
    parser.parse(&db).await?;
    txn.commit()?;
    Ok(db)
}

/// Ingest into an existing catalog under an explicit `dataset_id`, as
/// `bidslake index --dataset-id` does — for tests about accumulating datasets across
/// runs, where the id and the root are asserted rather than inferred.
pub async fn ingest_into(
    db: &BidsDb,
    dataset_path: impl AsRef<Path>,
    dataset_id: &str,
) -> Result<()> {
    let schema = Schema::load(None).unwrap();
    db.create_tables(&schema)?;
    let fs = Box::new(LocalFileSystem::new(dataset_path.as_ref().to_path_buf()));
    let mut parser = BidsParser::new(fs, Some(dataset_id.to_string()), schema, None, true, true);
    let txn = db.conn.unchecked_transaction()?;
    parser.parse(db).await?;
    txn.commit()?;
    Ok(())
}

/// Ingest into an existing catalog with **no** `--dataset-id`, so the id is inferred from
/// `dataset_description.json`'s `Name` (or the directory basename). The distinction from
/// [`ingest_into`] is load-bearing for a second root: an asserted id is the user's claim
/// that this root belongs to that dataset, an inferred one is only a `Name`, which
/// pipelines reuse across every study they process (docs/adr/0005).
pub async fn ingest_inferred_into(db: &BidsDb, dataset_path: impl AsRef<Path>) -> Result<()> {
    let schema = Schema::load(None).unwrap();
    db.create_tables(&schema)?;
    let fs = Box::new(LocalFileSystem::new(dataset_path.as_ref().to_path_buf()));
    let mut parser = BidsParser::new(fs, None, schema, None, true, true);
    let txn = db.conn.unchecked_transaction()?;
    parser.parse(db).await?;
    txn.commit()?;
    Ok(())
}

/// A fresh catalog holding one dataset, ingested under an explicit `dataset_id`.
pub async fn ingest_as(dataset_path: impl AsRef<Path>, dataset_id: &str) -> Result<BidsDb> {
    let db = BidsDb::new(":memory:")?;
    ingest_into(&db, dataset_path, dataset_id).await?;
    Ok(db)
}

/// Like [`ingest`], but with a caller-provided schema — e.g. one built via
/// `Schema::load_with_overlays` so tests can exercise overlay-augmented indexing.
pub async fn ingest_with_schema(dataset_path: impl AsRef<Path>, schema: Schema) -> Result<BidsDb> {
    let db = BidsDb::new(":memory:")?;
    db.create_tables(&schema)?;
    let fs = Box::new(LocalFileSystem::new(dataset_path.as_ref().to_path_buf()));
    let mut parser = BidsParser::new(fs, None, schema, None, true, true);
    let txn = db.conn.unchecked_transaction()?;
    parser.parse(&db).await?;
    txn.commit()?;
    Ok(db)
}

/// Ingest a standardized *non-BIDS* dataset with one or more bundled adapters (e.g.
/// `freesurfer`), mirroring `main::run_indexer`: each adapter contributes an overlay
/// (tables), a term map (projection), and an ingestion fragment (read/catalog policy),
/// which the schema-driven pipeline uses. Provenance is stamped.
pub async fn ingest_with_adapters(
    dataset_path: impl AsRef<Path>,
    adapter_names: &[&str],
) -> Result<BidsDb> {
    let db = BidsDb::new(":memory:")?;
    ingest_with_adapters_into(&db, dataset_path, adapter_names, None).await?;
    Ok(db)
}

/// A fresh adapter catalog holding one dataset under an explicit `dataset_id`.
pub async fn ingest_with_adapters_as(
    dataset_path: impl AsRef<Path>,
    adapter_names: &[&str],
    dataset_id: &str,
) -> Result<BidsDb> {
    let db = BidsDb::new(":memory:")?;
    ingest_with_adapters_into(&db, dataset_path, adapter_names, Some(dataset_id)).await?;
    Ok(db)
}

/// Ingest with adapters into an *existing* catalog — for tests about re-indexing, where the
/// second run must rebuild what the first wrote rather than append to it.
pub async fn ingest_with_adapters_into(
    db: &BidsDb,
    dataset_path: impl AsRef<Path>,
    adapter_names: &[&str],
    dataset_id: Option<&str>,
) -> Result<()> {
    let mut overlays: Vec<AppliedOverlay> = Vec::new();
    let mut term_maps: Vec<TermMap> = Vec::new();
    let mut ingestion_sources: Vec<String> = vec![
        bids_schema::bundled_ingestion_source("base")
            .unwrap()
            .to_string(),
    ];
    let mut term_map_prov: Vec<(String, serde_json::Value)> = Vec::new();
    let mut ingestion_prov: Vec<(String, serde_json::Value)> = Vec::new();
    // Each artifact is optional, as it is for `--adapter` itself (`main::resolve_adapters`): a
    // producer whose files are BIDS-named needs no term map (`fmriprep`), and one that only
    // scopes a policy needs no overlay (`dcmstack`). Requiring all three here would make the
    // helper stricter than the CLI and put a legitimate adapter combination out of test reach.
    for name in adapter_names {
        if let Some(content) = bids_schema::overlay::bundled_overlay(name) {
            overlays.push(AppliedOverlay {
                source: name.to_string(),
                content,
            });
        }
        if let Some(tm) = bundled_term_map(name) {
            term_maps.push(tm);
            let src = bids_schema::term_map::bundled_term_map_source(name).unwrap();
            term_map_prov.push((name.to_string(), serde_json::from_str(src).unwrap()));
        }
        if let Some(ing) = bids_schema::bundled_ingestion_source(name) {
            ingestion_sources.push(ing.to_string());
            ingestion_prov.push((name.to_string(), serde_json::from_str(ing).unwrap()));
        }
    }
    let ingestion = Ingestion::from_sources(
        &ingestion_sources
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
    )?;
    // Term maps must reach schema generation, not just the parser: they decide which
    // concept columns fall back from a stored projection, and that is a DDL question.
    let schema = Schema::load_full(None, &overlays, ingestion, &term_maps)?;

    db.create_tables(&schema)?;
    db.stamp_term_maps(&term_map_prov)?;
    db.stamp_ingestion(&ingestion_prov)?;

    let fs = Box::new(LocalFileSystem::new(dataset_path.as_ref().to_path_buf()));
    let mut parser = BidsParser::new(fs, dataset_id.map(str::to_string), schema, None, true, true)
        .with_term_maps(term_maps);
    let txn = db.conn.unchecked_transaction()?;
    parser.parse(db).await?;
    txn.commit()?;
    Ok(())
}

/// Write `content` to `root/rel`, creating the parent directories.
///
/// Panics rather than returning, because a fixture that cannot be written is a broken test
/// rather than a failing one — the Arrange phase, where a failure should error.
pub fn write(root: &Path, rel: &str, content: &[u8]) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().expect("a relative path has a parent"))
        .unwrap_or_else(|e| panic!("mkdir for {}: {e}", path.display()));
    std::fs::write(&path, content).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
}

/// `COUNT(*)` for a table.
pub fn count(db: &BidsDb, table: &str) -> Result<i64> {
    let sql = format!("SELECT COUNT(*) FROM {table}");
    Ok(db.conn.query_row(&sql, [], |r| r.get(0))?)
}

/// How many primary **data files** the catalog holds.
///
/// The thing tests used to spell `count(db, "scans")`. Since docs/adr/0006 that is the wrong
/// relation twice over: the registry holds every *kind* of file, and `scans` holds only the
/// data files a `scans.tsv` describes — none at all for a dataset that ships no `scans.tsv`,
/// which is most fixtures.
#[allow(dead_code)]
pub fn count_data_files(db: &BidsDb) -> Result<i64> {
    Ok(db.conn.query_row(
        "SELECT COUNT(*) FROM file_registry WHERE kind = 'data'",
        [],
        |r| r.get(0),
    )?)
}

/// Every file under `root` that ingest would *see* — the set the file registry is meant to
/// mirror (docs/adr/0006).
///
/// Uses the very same `bids_core::filetree::read_file_tree` walker as ingestion, so the
/// expected set applies dotfile, `.bidsignore` (including nested ones) and always-ignore
/// rules exactly as the walk does and cannot drift from it. Paths are dataset-relative,
/// matching `file_registry.file_path`.
pub fn walk_all(root: &Path) -> Vec<String> {
    let schema: serde_json::Value = serde_json::from_str(bids_schema::SCHEMA_JSON).unwrap();
    let pseudo_exts = bids_schema::pseudo_file_extensions(&schema);
    let tree = bids_core::filetree::read_file_tree(root, &pseudo_exts, true)
        .unwrap_or_else(|e| panic!("read_file_tree({}) failed: {e}", root.display()));
    tree.walk_files()
        .map(|f| f.path.trim_start_matches('/').to_string())
        .collect()
}

/// Every tabular file (`.tsv`/`.tsv.gz`) under `root` that ingest would *see*.
/// Uses the very same `bids_core::filetree::read_file_tree` walker as ingestion —
/// which applies dotfile, `.bidsignore` (including nested ones), and always-ignore
/// (`.git`/`.datalad`/…) rules during the walk — so this expected set cannot drift
/// from what ingest actually walks. Paths are root-relative, matching
/// `file_registry.file_path`.
pub fn walk_tabular(root: &Path) -> Vec<String> {
    let schema: serde_json::Value = serde_json::from_str(bids_schema::SCHEMA_JSON).unwrap();
    let pseudo_exts = bids_schema::pseudo_file_extensions(&schema);
    let tree = bids_core::filetree::read_file_tree(root, &pseudo_exts, true)
        .unwrap_or_else(|e| panic!("read_file_tree({}) failed: {e}", root.display()));
    tree.walk_files()
        .map(|f| f.path.trim_start_matches('/').to_string())
        .filter(|p| p.ends_with(".tsv") || p.ends_with(".tsv.gz"))
        .collect()
}

/// Absolute path to the vendored `bids-examples` submodule.
pub fn bids_examples_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/bids-examples")
}

/// Path to a single dataset inside `bids-examples`.
pub fn bids_example(name: &str) -> PathBuf {
    bids_examples_dir().join(name)
}

/// Datasets deliberately excluded from ingestion. `genetics_ukbb` carries
/// genetic/UK-Biobank-style data that we do not process.
pub const EXCLUDED_DATASETS: &[&str] = &["genetics_ukbb"];

/// Whether a dataset name is on the exclusion list (see [`EXCLUDED_DATASETS`]).
pub fn is_excluded(name: &str) -> bool {
    EXCLUDED_DATASETS.contains(&name) || name.starts_with("genetics")
}

/// Every dataset directory in `bids-examples` — i.e. immediate subdirectories
/// that contain a `dataset_description.json` and are not excluded. Returns
/// `(name, path)` sorted by name. Empty (with a clear panic) if the submodule
/// has not been checked out.
pub fn all_datasets() -> Vec<(String, PathBuf)> {
    let root = bids_examples_dir();
    let entries = std::fs::read_dir(&root).unwrap_or_else(|e| {
        panic!(
            "cannot read bids-examples at {} ({e}). Run `git submodule update --init`.",
            root.display()
        )
    });

    let mut datasets: Vec<(String, PathBuf)> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.join("dataset_description.json").is_file())
        .filter_map(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| (n.to_string(), p.clone()))
        })
        .filter(|(name, _)| !is_excluded(name))
        .collect();

    assert!(
        !datasets.is_empty(),
        "no datasets found under {}. Run `git submodule update --init`.",
        root.display()
    );

    datasets.sort_by(|a, b| a.0.cmp(&b.0));
    datasets
}
