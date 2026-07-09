//! Ingestion throughput benchmark.
//!
//! Ingests a fixed subset of the `bids-examples` corpus into an in-memory
//! DuckDB and measures wall time. `parse` is async, so each iteration drives it
//! on a fresh Tokio runtime.
//!
//! Run with `cargo bench` (requires `git submodule update --init`).

use std::path::{Path, PathBuf};

use bidslake::{bids::BidsParser, db::BidsDb, fs::LocalFileSystem, schema::Schema};
use criterion::{criterion_group, criterion_main, Criterion};

/// Small, stable datasets that exercise the common paths (anat/func, sessions,
/// events, inheritance) without dominating the benchmark with one huge dataset.
const DATASETS: &[&str] = &["ds001", "ds002", "ds114"];

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
        let schema = Schema::load(None);
        db.create_tables(&schema).expect("create tables");
        let fs = Box::new(LocalFileSystem::new(path.to_path_buf()));
        let mut parser = BidsParser::new(fs, None, schema);
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

criterion_group!(benches, bench_ingest);
criterion_main!(benches);
