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
//! Run with `cargo bench` (requires `git submodule update --init`).

use std::path::{Path, PathBuf};

use bidslake::{
    bids::{BidsParser, S3Httpfs},
    db::BidsDb,
    fs::LocalFileSystem,
    s3,
    schema::Schema,
};
use criterion::{Criterion, criterion_group, criterion_main};

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

criterion_group!(benches, bench_ingest, bench_ingest_s3);
criterion_main!(benches);
