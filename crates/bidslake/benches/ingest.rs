//! Ingestion throughput benchmark.
//!
//! The `ingest` group ingests a fixed subset of the `bids-examples` corpus into
//! an in-memory DuckDB and measures wall time — CPU-bound regression tracking for
//! the local path. `parse` is async, so each iteration drives it on a fresh Tokio
//! runtime.
//!
//! The `ingest_s3` group ingests an OpenNeuro dataset **straight from S3** — the
//! network-latency benchmark for identifying I/O speed issues (which are invisible
//! on warm local disk). It is opt-in because it hits the network: set
//! `BIDSLAKE_S3_BENCH=1` (and optionally `BIDSLAKE_S3_DATASET=ds000001`) to run it.
//! Pair it with `BIDSLAKE_TIMING=1` on a real `index` run for the phase breakdown.
//!
//! The `ingest_latency` group ingests a synthetic diffusion-rich tree through
//! [`SlowFs`], which injects a fixed per-read delay to simulate network RTT
//! deterministically (no network). It guards the concurrent Rust-side reads: the
//! prefetch issues the bval/bvec/sidecar reads concurrently, so wall time tracks
//! `ceil(reads / 16) * delay`, not `reads * delay` — a regression alarm if those
//! reads ever go sequential again. This win is invisible on warm local disk, hence
//! the injected latency.
//!
//! Run with `cargo bench` (requires `git submodule update --init`).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use bids_core::filetree::FileTree;
use bidslake::{
    bids::{BidsParser, S3Httpfs},
    db::BidsDb,
    fs::{BidsFileSystem, LocalFileSystem},
    s3,
    schema::Schema,
};
use criterion::{Criterion, criterion_group, criterion_main};
use futures::future::BoxFuture;

/// Datasets chosen to exercise the cost drivers: `ds001`/`ds002`/`ds114` cover
/// the common paths (anat/func, sessions, events, inheritance); `ds108` is
/// insert-heavy (≈238 scans, ≈200 `_events.tsv`), so it guards the bulk-Appender
/// path for `scans`/`sidecars` and the per-file tabular `read_csv` cost against
/// regressions.
const DATASETS: &[&str] = &["ds001", "ds002", "ds114", "ds108"];

fn bids_example(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/bids-examples")
        .join(name)
}

/// Ingest one dataset into a fresh in-memory database. The schema is loaded once
/// per iteration to mirror a real `index` invocation.
fn ingest_once(path: &Path) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(async {
        let db = BidsDb::new(":memory:").expect("open db");
        let schema = Schema::load(None).expect("load schema");
        db.create_tables(&schema).expect("create tables");
        let fs = Box::new(LocalFileSystem::new(path.to_path_buf()));
        let mut parser = BidsParser::new(fs, None, schema, None, true);
        parser.parse(&db).await.expect("parse");
    });
}

fn bench_ingest(c: &mut Criterion) {
    let mut group = c.benchmark_group("ingest");
    // These datasets are small but the wide sidecars table makes each ingest a
    // few hundred ms; keep the sample size modest so `cargo bench` stays quick.
    group.sample_size(10);

    for name in DATASETS {
        let path = bids_example(name);
        if !path.join("dataset_description.json").is_file() {
            eprintln!(
                "skipping {name}: {} not found (run `git submodule update --init`)",
                path.display()
            );
            continue;
        }
        group.bench_function(*name, |b| b.iter(|| ingest_once(&path)));
    }

    group.finish();
}

/// Ingest one OpenNeuro dataset from S3 (anonymous), with httpfs on both the
/// write and preflight connections — mirrors `main`'s S3 path.
fn ingest_s3_once(dataset: &str) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(async {
        let db = BidsDb::new(":memory:").expect("open db");
        let schema = Schema::load(None).expect("load schema");
        db.create_tables(&schema).expect("create tables");
        let client = s3::S3Client::new("openneuro.org", dataset, s3::SigningMode::Anonymous)
            .await
            .expect("s3 client");
        let region = client.region().to_string();
        s3::configure_httpfs(&db.conn, &region, true).expect("httpfs");
        let mut parser = BidsParser::new(
            Box::new(client),
            Some(dataset.to_string()),
            schema,
            Some(S3Httpfs {
                region: region.clone(),
                anonymous: true,
            }),
            true,
        );
        parser.parse(&db).await.expect("parse");
    });
}

/// Opt-in network benchmark: ingest a dataset from S3. Skipped unless
/// `BIDSLAKE_S3_BENCH` is set, since it hits the network and installs httpfs.
fn bench_ingest_s3(c: &mut Criterion) {
    if std::env::var_os("BIDSLAKE_S3_BENCH").is_none() {
        return;
    }
    let dataset = std::env::var("BIDSLAKE_S3_DATASET").unwrap_or_else(|_| "ds000001".to_string());
    let mut group = c.benchmark_group("ingest_s3");
    // Network ingests are seconds each; keep the sample count at criterion's floor.
    group.sample_size(10);
    group.bench_function(&dataset, |b| b.iter(|| ingest_s3_once(&dataset)));
    group.finish();
}

/// A [`LocalFileSystem`] that sleeps `delay` before each Rust-side read
/// (`read_to_string` / `read_head` — the reads the prefetch parallelizes),
/// simulating network round-trip latency deterministically. `walk` and
/// `read_csv_source` (DuckDB's, not the prefetch's) delegate without delay.
struct SlowFs {
    inner: LocalFileSystem,
    delay: Duration,
}

impl SlowFs {
    fn new(inner: LocalFileSystem, delay: Duration) -> Self {
        Self { inner, delay }
    }
}

impl BidsFileSystem for SlowFs {
    fn walk(
        &self,
        pseudo_exts: &[String],
        apply_bidsignore: bool,
    ) -> BoxFuture<'_, Result<Vec<PathBuf>>> {
        self.inner.walk(pseudo_exts, apply_bidsignore)
    }

    fn read_to_string(&self, path: &Path) -> BoxFuture<'_, Result<String>> {
        let delay = self.delay;
        let inner = &self.inner;
        let path = path.to_path_buf();
        Box::pin(async move {
            tokio::time::sleep(delay).await;
            inner.read_to_string(&path).await
        })
    }

    fn read_head(&self, path: &Path, max_bytes: usize) -> BoxFuture<'_, Result<String>> {
        let delay = self.delay;
        let inner = &self.inner;
        let path = path.to_path_buf();
        Box::pin(async move {
            tokio::time::sleep(delay).await;
            inner.read_head(&path, max_bytes).await
        })
    }

    fn read_csv_source(&self, path: &Path) -> BoxFuture<'_, Result<String>> {
        self.inner.read_csv_source(path)
    }

    fn root(&self) -> String {
        self.inner.root()
    }

    fn file_tree(&self) -> Option<Arc<FileTree>> {
        self.inner.file_tree()
    }
}

/// Build a synthetic diffusion dataset: `n` subjects, each a `dwi/*_dwi.{bval,bvec,json}`
/// triple. That is `3n` bodies the passes read in Rust (bval + bvec + sidecar), so the
/// concurrent prefetch's win over sequential reads is clear once `SlowFs` adds latency.
fn write_diffusion_tree(root: &Path, n: usize) {
    std::fs::write(
        root.join("dataset_description.json"),
        br#"{"Name": "slowfs-bench", "BIDSVersion": "1.8.0"}"#,
    )
    .expect("write dataset_description");
    for i in 0..n {
        let sub = format!("sub-{:03}", i + 1);
        let dwi = root.join(&sub).join("dwi");
        std::fs::create_dir_all(&dwi).expect("mkdir dwi");
        let base = format!("{sub}_dwi");
        // 4 volumes; bvec is 3 rows (x/y/z) of 4 — a valid, minimal pair.
        std::fs::write(dwi.join(format!("{base}.bval")), b"0 1000 2000 3000\n").expect("bval");
        std::fs::write(
            dwi.join(format!("{base}.bvec")),
            b"0 1 0 0\n0 0 1 0\n0 0 0 1\n",
        )
        .expect("bvec");
        std::fs::write(
            dwi.join(format!("{base}.json")),
            br#"{"PhaseEncodingDirection": "j-"}"#,
        )
        .expect("sidecar");
    }
}

/// Ingest one dataset into a fresh in-memory database through an arbitrary backend.
fn ingest_through(fs: Box<dyn BidsFileSystem>) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(async {
        let db = BidsDb::new(":memory:").expect("open db");
        let schema = Schema::load(None).expect("load schema");
        db.create_tables(&schema).expect("create tables");
        let mut parser = BidsParser::new(fs, None, schema, None, true);
        parser.parse(&db).await.expect("parse");
    });
}

fn bench_ingest_latency(c: &mut Criterion) {
    let dir = tempfile::tempdir().expect("tempdir");
    let n = 32;
    write_diffusion_tree(dir.path(), n);
    // 5 ms/read over `3n` reads: concurrent (cap 16) ≈ ceil(3n/16)·5 ms on top of
    // the fixed schema/DuckDB cost; a regression to sequential would add ~3n·5 ms
    // (here ~480 ms), an unmistakable jump.
    let delay = Duration::from_millis(5);

    let mut group = c.benchmark_group("ingest_latency");
    group.sample_size(10);
    group.bench_function(format!("dwi_{n}subj_{}ms", delay.as_millis()), |b| {
        b.iter(|| {
            let fs = Box::new(SlowFs::new(
                LocalFileSystem::new(dir.path().to_path_buf()),
                delay,
            ));
            ingest_through(fs);
        })
    });
    group.finish();
}

criterion_group!(benches, bench_ingest, bench_ingest_s3, bench_ingest_latency);
criterion_main!(benches);
