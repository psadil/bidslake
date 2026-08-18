#![allow(missing_docs)] // `criterion_main!` generates an undocumented `main`.

//! Ingestion throughput over a generated tree.
//!
//! `crates/bidslake/benches/ingest.rs` measures the `bids-examples` corpus, whose widest tabular
//! header is about eleven columns and none of whose datasets ships a `scans.tsv` or a
//! `sessions.tsv`. A regression in the batched tabular path therefore costs nothing any
//! benchmark could see. This one closes that: an fMRIPrep tree at the measured real confounds
//! shape, 1,841 columns by 450 rows, plus a raw tree that carries all three modality-agnostic
//! tables.
//!
//! **The overlay is not optional.** Without the `fmriprep` overlay merged, a confounds file
//! routes nowhere and is recorded `skipped`, so the benchmark would measure the directory walk
//! and report it as tabular throughput. That is the exact gap this case exists to close, which
//! is why the schema is built explicitly rather than with `Schema::load`.

use std::path::Path;

use bidslake::schema::{AppliedOverlay, Schema};
use bidslake::{bids::BidsParser, db::BidsDb, fs::LocalFileSystem};
use bidslake_synth::{Plan, Producer, Scale};
use criterion::{Criterion, criterion_group, criterion_main};

fn fmriprep_schema() -> Schema {
    let overlay = AppliedOverlay {
        source: "fmriprep".to_string(),
        content: bids_schema::overlay::bundled_overlay("fmriprep").expect("bundled overlay"),
    };
    Schema::load_with_overlays(None, &[overlay]).expect("schema loads")
}

/// Ingest one tree into a fresh in-memory database, mirroring what `index` does per run.
fn ingest_once(path: &Path, schema: &Schema) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(async {
        let db = BidsDb::new(":memory:").expect("open db");
        db.create_tables(schema).expect("create tables");
        let fs = Box::new(LocalFileSystem::new(path.to_path_buf()));
        let mut parser = BidsParser::new(fs, None, schema.clone(), None, true, true);
        parser.parse(&db).await.expect("parse");
    });
}

fn bench_ingest_wide_tabular(c: &mut Criterion) {
    let schema = fmriprep_schema();
    let dir = tempfile::tempdir().expect("tempdir");
    let scale = Scale {
        subjects: 4,
        sessions: 1,
        runs: 2,
        ..Scale::default()
    }
    .a2cps_melodic();
    let manifest = Plan::producer(Producer::Fmriprep)
        .scale(scale)
        .write(dir.path(), &schema)
        .expect("writes");

    let mut group = c.benchmark_group("ingest_wide_tabular");
    group.sample_size(10);
    group.bench_function(
        format!("confounds_1841x450_{}files", manifest.files.len()),
        |b| b.iter(|| ingest_once(dir.path(), &schema)),
    );
    group.finish();
}

fn bench_ingest_raw_tables(c: &mut Criterion) {
    let schema = Schema::load(None).expect("schema loads");
    let dir = tempfile::tempdir().expect("tempdir");
    let scale = Scale {
        subjects: 16,
        sessions: 2,
        runs: 4,
        confound_rows: 32,
        ..Scale::default()
    };
    let manifest = Plan::producer(Producer::Raw)
        .scale(scale)
        .write(dir.path(), &schema)
        .expect("writes");

    let mut group = c.benchmark_group("ingest_raw_tables");
    group.sample_size(10);
    group.bench_function(
        format!("scans_sessions_participants_{}files", manifest.files.len()),
        |b| b.iter(|| ingest_once(dir.path(), &schema)),
    );
    group.finish();
}

criterion_group!(benches, bench_ingest_wide_tabular, bench_ingest_raw_tables);
criterion_main!(benches);
