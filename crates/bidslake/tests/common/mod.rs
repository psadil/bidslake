//! Shared helpers for integration tests.
//!
//! Included via `mod common;` in each test file. Living under `tests/common/`
//! (a subdirectory) keeps Cargo from compiling it as its own test binary.
//!
//! Not every test binary uses every helper, so suppress the cross-binary
//! "never used" warnings that Cargo's per-binary compilation produces.
#![allow(dead_code)]

use anyhow::Result;
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
    let mut parser = BidsParser::new(fs, None, schema, None, true);
    let txn = db.conn.unchecked_transaction()?;
    parser.parse(&db).await?;
    txn.commit()?;
    Ok(db)
}

/// Like [`ingest`], but with a caller-provided schema — e.g. one built via
/// `Schema::load_with_overlays` so tests can exercise overlay-augmented indexing.
pub async fn ingest_with_schema(dataset_path: impl AsRef<Path>, schema: Schema) -> Result<BidsDb> {
    let db = BidsDb::new(":memory:")?;
    db.create_tables(&schema)?;
    let fs = Box::new(LocalFileSystem::new(dataset_path.as_ref().to_path_buf()));
    let mut parser = BidsParser::new(fs, None, schema, None, true);
    let txn = db.conn.unchecked_transaction()?;
    parser.parse(&db).await?;
    txn.commit()?;
    Ok(db)
}

/// `COUNT(*)` for a table.
pub fn count(db: &BidsDb, table: &str) -> Result<i64> {
    let sql = format!("SELECT COUNT(*) FROM {table}");
    Ok(db.conn.query_row(&sql, [], |r| r.get(0))?)
}

/// Every tabular file (`.tsv`/`.tsv.gz`) under `root` that ingest would *see*.
/// Uses the very same `bids_core::filetree::read_file_tree` walker as ingestion —
/// which applies dotfile, `.bidsignore` (including nested ones), and always-ignore
/// (`.git`/`.datalad`/…) rules during the walk — so this expected set cannot drift
/// from what ingest actually walks. Paths are dataset-relative, matching
/// `tabular_files.file_path`.
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
