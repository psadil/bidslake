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

use bids_schema::term_map::{TermMap, bundled_term_map};
use bidslake::schema::{AppliedOverlay, Ingestion, Schema};
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

/// The full artifact set a bundled adapter applies: overlay, term map and ingestion fragment.
///
/// Assembled here rather than with `Schema::load` because a layout adapter is exactly the case
/// `Schema::load` cannot express — and until this benchmark existed, nothing in the repo
/// measured one. Both existing benchmark files build a base schema, so a FreeSurfer or FEAT
/// tree routed nowhere: the term-map dispatch, the `read`-disposition content readers and the
/// batched writes that serve them had no regression guard at all.
fn adapter_schema(name: &str) -> Schema {
    let overlays: Vec<AppliedOverlay> = bids_schema::overlay::bundled_overlay(name)
        .map(|content| AppliedOverlay {
            source: name.to_string(),
            content,
        })
        .into_iter()
        .collect();
    let term_maps: Vec<TermMap> = bundled_term_map(name).into_iter().collect();
    let mut sources = vec![bids_schema::bundled_ingestion_source("base").expect("base ingestion")];
    if let Some(ing) = bids_schema::bundled_ingestion_source(name) {
        sources.push(ing);
    }
    let ingestion = Ingestion::from_sources(&sources).expect("ingestion merges");
    Schema::load_full(None, &overlays, ingestion, &term_maps).expect("adapter schema loads")
}

/// Ingest a layout tree through its adapter, as `bidslake index --adapter <name>` does.
fn ingest_adapter_once(path: &Path, schema: &Schema, term_map_name: &str) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(async {
        let db = BidsDb::new(":memory:").expect("open db");
        db.create_tables(schema).expect("create tables");
        let fs = Box::new(LocalFileSystem::new(path.to_path_buf()));
        // Rebuilt per iteration rather than cloned: `TermMap` is not `Clone`, and loading a
        // bundled one is a parse of a small document that the measured ingest dwarfs.
        let term_maps: Vec<TermMap> = bundled_term_map(term_map_name).into_iter().collect();
        let mut parser =
            BidsParser::new(fs, None, schema.clone(), None, true, true).with_term_maps(term_maps);
        parser.parse(&db).await.expect("parse");
    });
}

/// A FreeSurfer `recon-all` tree read through the `freesurfer` adapter.
///
/// The shape this covers that nothing else does: every file is claimed by a term map rather
/// than by a BIDS filename, and the `.stats` files are `read` in Rust and written through the
/// content-reader path — which is per-file work that the BIDS tabular path never touches.
fn bench_ingest_freesurfer_adapter(c: &mut Criterion) {
    let schema = adapter_schema("freesurfer");
    let dir = tempfile::tempdir().expect("tempdir");
    let layout = bids_schema::layout::bundled_layout("freesurfer").expect("bundled layout");
    let manifest = Plan::producer(Producer::Layout(Box::new(layout)))
        .scale(Scale {
            subjects: 8,
            ..Scale::default()
        })
        .write(dir.path(), &schema)
        .expect("writes");

    let mut group = c.benchmark_group("ingest_adapter");
    group.sample_size(10);
    group.bench_function(format!("freesurfer_{}files", manifest.files.len()), |b| {
        b.iter(|| ingest_adapter_once(dir.path(), &schema, "freesurfer"))
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_ingest_wide_tabular,
    bench_ingest_raw_tables,
    bench_ingest_freesurfer_adapter
);
criterion_main!(benches);
