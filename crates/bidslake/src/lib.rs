//! # bidslake — a lakehouse for BIDS datasets
//!
//! bidslake consolidates the metadata of a [BIDS](https://bids-specification.readthedocs.io/)
//! neuroimaging dataset — scattered across JSON sidecars, `.tsv` tables, and
//! filename entities — into a single [DuckDB](https://duckdb.org/) database,
//! while the bulky NIfTI files stay on disk. You then query and edit the dataset
//! with SQL.
//!
//! ## Pipeline at a glance
//!
//! [`schema::Schema::load`] parses the vendored BIDS `schema.json` and generates
//! the DuckDB table definitions from it. [`db::BidsDb`] opens the database and
//! creates those tables. [`bids::BidsParser`] then walks the dataset (local or
//! S3 via the [`fs::BidsFileSystem`] abstraction), parses sidecars / TSVs /
//! filename entities, and writes rows — the whole ingest wrapped in one
//! transaction. The [`schema`] module is the table/column reference; for *how*
//! that schema is generated from the BIDS schema read [`schema::dynamic`], and
//! for the ingestion pipeline read [`bids`]. (The README covers the pitch and the
//! managed-mode design.)
//!
//! # Examples
//!
//! Each example below is a doctest — it runs against the vendored
//! `bids-examples` corpus (fetch it with `git submodule update --init`), so the
//! usage shown here can't drift from the code.
//!
//! ## Select files, then iterate over them
//!
//! The everyday task: ask bidslake for the files you want, get a list of paths,
//! and hand each to your tool of choice. Here, the `T1w` images of participants
//! under 30:
//!
//! ```
//! # use bidslake::{bids::BidsParser, db::BidsDb, fs::LocalFileSystem, schema::Schema};
//! # tokio::runtime::Runtime::new().unwrap().block_on(async {
//! let db = BidsDb::new(":memory:")?;
//! let schema = Schema::load(None)?;
//! db.create_tables(&schema)?;
//!
//! let dataset = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/bids-examples/ds001");
//! let fs = Box::new(LocalFileSystem::new(dataset));
//! BidsParser::new(fs, None, schema, None, true).parse(&db).await?;
//!
//! let mut stmt = db.conn.prepare(
//!     "SELECT s.file_path \
//!      FROM participants p \
//!      JOIN all_files s ON s.dataset_id = p.dataset_id \
//!                  AND s.file_path LIKE p.participant_id || '/%' \
//!      WHERE p.age < 30 AND s.suffix = 'T1w' \
//!      ORDER BY s.file_path",
//! )?;
//! let files: Vec<String> = stmt.query_map([], |row| row.get(0))?
//!     .collect::<Result<_, _>>()?;
//!
//! // `files` is just a list of paths on disk. Iterate and pass each to your
//! // pipeline (nibabel, nilearn, FSL, ANTs, ...); bidslake does not touch the
//! // image data itself.
//! for _path in &files {
//!     // e.g. load `_path` with nibabel / nilearn and preprocess it
//! }
//! assert!(!files.is_empty());
//! # Ok::<(), anyhow::Error>(())
//! # }).unwrap();
//! ```
//!
//! ## Query by BIDS concept, across datasets
//!
//! `all_files` — the file registry — exposes BIDS entities plus
//! `datatype`/`suffix`/`modality` as derived columns, so you filter by concept instead
//! of matching path substrings. It holds every file the walk saw, so add
//! `kind = 'data'` to narrow it to primary data files. And because every row carries a
//! `dataset_id`, many datasets can share one database and be queried together — `ses` is
//! simply `NULL` for datasets that don't use sessions, so one query spans a mixed pool.
//!
//! ```
//! # use bidslake::{bids::BidsParser, db::BidsDb, fs::LocalFileSystem, schema::Schema};
//! # tokio::runtime::Runtime::new().unwrap().block_on(async {
//! let db = BidsDb::new(":memory:")?;
//! let schema = Schema::load(None)?;
//! db.create_tables(&schema)?;
//!
//! // Aggregate two datasets into one database (ds001 has no sessions; ds114 does).
//! for name in ["ds001", "ds114"] {
//!     let path = format!("{}/tests/bids-examples/{}", env!("CARGO_MANIFEST_DIR"), name);
//!     let fs = Box::new(LocalFileSystem::new(path));
//!     BidsParser::new(fs, Some(name.to_string()), schema.clone(), None, true).parse(&db).await?;
//! }
//!
//! // Functional BOLD runs across both datasets — by concept, not by path.
//! let bold_runs: i64 = db.conn.query_row(
//!     "SELECT COUNT(*) FROM all_files WHERE datatype = 'func' AND suffix = 'bold'",
//!     [], |r| r.get(0),
//! )?;
//! assert!(bold_runs > 0);
//! # Ok::<(), anyhow::Error>(())
//! # }).unwrap();
//! ```
//!
//! ## Edit metadata with one statement
//!
//! Renaming a participant is a SQL `UPDATE` — not a directory-and-filename rewrite
//! across the whole tree.
//!
//! ```
//! # use bidslake::{bids::BidsParser, db::BidsDb, fs::LocalFileSystem, schema::Schema};
//! # tokio::runtime::Runtime::new().unwrap().block_on(async {
//! # let db = BidsDb::new(":memory:")?;
//! # let schema = Schema::load(None)?;
//! # db.create_tables(&schema)?;
//! # let dataset = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/bids-examples/ds001");
//! # BidsParser::new(Box::new(LocalFileSystem::new(dataset)), None, schema, None, true).parse(&db).await?;
//! db.conn.execute(
//!     "UPDATE participants SET participant_id = 'sub-001' WHERE participant_id = 'sub-01'",
//!     [],
//! )?;
//! let renamed: i64 = db.conn.query_row(
//!     "SELECT COUNT(*) FROM participants WHERE participant_id = 'sub-001'",
//!     [], |r| r.get(0),
//! )?;
//! assert_eq!(renamed, 1);
//! # Ok::<(), anyhow::Error>(())
//! # }).unwrap();
//! ```
//!
//! ## Find associated files (e.g. fieldmaps for SDC)
//!
//! `IntendedFor` links are resolved at ingest into `file_associations`, so pairing
//! each scan with the fieldmap intended for it — what susceptibility distortion
//! correction needs — is a query.
//!
//! ```
//! # use bidslake::{bids::BidsParser, db::BidsDb, fs::LocalFileSystem, schema::Schema};
//! # tokio::runtime::Runtime::new().unwrap().block_on(async {
//! # let db = BidsDb::new(":memory:")?;
//! # let schema = Schema::load(None)?;
//! # db.create_tables(&schema)?;
//! # let dataset = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/bids-examples/2d_mb_pcasl");
//! # BidsParser::new(Box::new(LocalFileSystem::new(dataset)), None, schema, None, true).parse(&db).await?;
//! let mut stmt = db.conn.prepare(
//!     "SELECT a.target_file_path, src.file_path \
//!      FROM file_associations a JOIN all_files src ON src.file_id = a.source_file_id \
//!      WHERE a.association_type = 'fieldmap'",
//! )?;
//! let pairs: Vec<(String, String)> = stmt
//!     .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
//!     .collect::<Result<_, _>>()?;
//!
//! // Each (target scan, fieldmap) pair is what you feed to an SDC step.
//! for (_scan, _fieldmap) in &pairs {}
//! assert!(!pairs.is_empty());
//! # Ok::<(), anyhow::Error>(())
//! # }).unwrap();
//! ```

/// This build: the package version, plus the commit it was built from and whether
/// the tree was dirty (see `build.rs`). Reported by `--version`, by the
/// `BIDSLAKE_TIMING` breakdown, and stamped into every catalog's `bidslake_meta`, so
/// a measurement can always be tied back to the code that produced it.
pub const BUILD: &str = env!("BIDSLAKE_BUILD");

pub mod bids;
pub mod compact;
pub mod db;
pub mod fs;
pub mod links;
pub mod readers;
/// S3-backed ingestion. Requires the `s3` feature (on by default); building with
/// `--no-default-features` drops it along with the AWS SDK.
#[cfg(feature = "s3")]
pub mod s3;
pub mod schema;
pub mod timing;
