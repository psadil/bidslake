//! What a generated tree owes the rest of the workspace.
//!
//! Five questions, and none of them is "are the columns the ones the schema declares" — that one
//! is unanswerable here, because the generator gets its columns from the same `Tabular::route`
//! call a test would check them against. Consistency is not correctness, and correctness lives
//! in the hand-written fixtures under `crates/bidslake/tests/`, where a human wrote the expected
//! values down.
//!
//! What *is* answerable, and is what these ask:
//!
//! 1. Does a generated raw tree pass `bids-validator-rs`? An independent implementation, with no
//!    knowledge of this crate, saying yes.
//! 2. Does an adapter tree get recognized rather than reported as un-BIDS?
//! 3. Do the generated bodies survive a *reader* — confounds into the typed table at the row
//!    count scale implies, and a `.stats` file into typed rows rather than a wall of NULLs?
//! 4. Does the walk see exactly the files the manifest claims were written?
//! 5. Does a layout loaded from a path generate its own tree, and does the same plan twice write
//!    the same bytes?

use std::collections::BTreeSet;

use bids_validator_rs::config::ValidatorConfig;
use bids_validator_rs::schema::BidsSchema;
use bidslake::db::BidsDb;
use bidslake::schema::{AppliedOverlay, Ingestion, Schema};
use bidslake::{bids::BidsParser, fs::LocalFileSystem};
use bidslake_synth::{Plan, Producer, Scale};

fn schema_with(adapters: &[&str]) -> Schema {
    let overlays: Vec<AppliedOverlay> = adapters
        .iter()
        .filter_map(|name| {
            bids_schema::overlay::bundled_overlay(name).map(|content| AppliedOverlay {
                source: (*name).to_string(),
                content,
            })
        })
        .collect();
    let mut sources = vec![bids_schema::bundled_ingestion_source("base").expect("base")];
    sources.extend(
        adapters
            .iter()
            .filter_map(|n| bids_schema::bundled_ingestion_source(n)),
    );
    let ingestion = Ingestion::from_sources(&sources).expect("ingestion loads");
    let term_maps: Vec<_> = adapters
        .iter()
        .filter_map(|n| bids_schema::term_map::bundled_term_map(n))
        .collect();
    Schema::load_full(None, &overlays, ingestion, &term_maps).expect("schema loads")
}

fn small() -> Scale {
    Scale {
        subjects: 2,
        sessions: 1,
        runs: 2,
        confound_rows: 6,
        ..Scale::default()
    }
}

/// The acceptance bar. `bids-validator-rs` shares no code with the generator and has never heard
/// of it, so this is the one check that is not the generator marking its own homework — and it
/// is the reason imaging files carry a real NIfTI header rather than nothing, and the reason
/// sidecar keys come from `rules.sidecars` rather than from a list.
///
/// Errors only. Warnings are all `*_RECOMMENDED`, which every minimal dataset earns and which a
/// generator would have to invent scanner metadata to silence.
#[tokio::test]
async fn a_generated_raw_tree_has_no_validator_errors() {
    let dir = tempfile::tempdir().expect("tempdir");
    let schema = schema_with(&[]);
    Plan::producer(Producer::Raw)
        .scale(small())
        .write(dir.path(), &schema)
        .expect("writes");

    let issues = bids_validator_rs::validate(
        dir.path(),
        &BidsSchema::bundled().expect("bundled validator schema"),
        None,
    )
    .await
    .expect("validator runs");

    let errors = issues.errors();
    assert!(
        errors.is_empty(),
        "{} validator errors:\n{}",
        errors.len(),
        issues.format_summary()
    );
}

/// The `Producer::Raw` bar, applied to a derivative tree — which was not previously possible.
///
/// Every other suffix this producer writes (`timeseries`, `xfm`, `boldref`) is overlay
/// vocabulary that no `rules.files` group describes, so the validator reports each one
/// `NotIncluded` and the tree could never be held to "zero errors". The surface family is the
/// first that *is* described: the always-applied `bep011` overlay carries
/// `rules.files.deriv.structural_mri`, mirroring the BEP the standard is adopting.
///
/// So this asserts on `NOT_INCLUDED` for the surface paths specifically rather than on the whole
/// error set, and that is the honest scope: it fails if the overlay's rules stop reaching them —
/// which is exactly what happens if the merge is dropped from `BidsSchema::bundled()`, the one
/// place the validator's schema and the indexer's could silently diverge.
#[tokio::test]
async fn a_generated_fmriprep_trees_surfaces_are_part_of_bids() {
    let dir = tempfile::tempdir().expect("tempdir");
    let schema = schema_with(&["fmriprep"]);
    Plan::producer(Producer::Fmriprep)
        .scale(small())
        .write(dir.path(), &schema)
        .expect("writes");

    let issues = bids_validator_rs::validate(
        dir.path(),
        &BidsSchema::bundled().expect("bundled validator schema"),
        None,
    )
    .await
    .expect("validator runs");

    let unrecognized: Vec<&str> = issues
        .all()
        .iter()
        .filter(|i| i.code == "NOT_INCLUDED")
        .map(|i| i.location.as_str())
        .filter(|p| p.ends_with(".shape.gii") || p.ends_with(".surf.gii"))
        .collect();

    assert!(
        unrecognized.is_empty(),
        "{} surface paths reported as not part of BIDS: {unrecognized:?}",
        unrecognized.len()
    );
}

/// ADR 0002 §7 at scale: a term-mapped file is *expected*, not un-BIDS. Without the adapter
/// configured every one of these paths is a `NotIncluded` error, so this asserts the suppression
/// reaches a whole generated tree and not only the handful of paths a fixture lists.
#[rstest::rstest]
#[case::freesurfer("freesurfer")]
#[case::feat("feat")]
#[tokio::test]
async fn a_generated_adapter_tree_is_recognized(#[case] adapter: &str) {
    let dir = tempfile::tempdir().expect("tempdir");
    let schema = schema_with(&[adapter]);
    let layout = bids_schema::layout::bundled_layout(adapter).expect("bundled layout");
    Plan::producer(Producer::Layout(Box::new(layout)))
        .scale(small())
        .write(dir.path(), &schema)
        .expect("writes");
    let config = ValidatorConfig {
        adapters: vec![adapter.to_string()],
        ..Default::default()
    };

    let issues = bids_validator_rs::validate(
        dir.path(),
        &BidsSchema::bundled().expect("bundled validator schema"),
        Some(&config),
    )
    .await
    .expect("validator runs");

    let not_included: Vec<&str> = issues
        .all()
        .iter()
        .filter(|i| i.code == "NOT_INCLUDED")
        .map(|i| i.location.as_str())
        .collect();
    assert!(
        not_included.is_empty(),
        "{adapter}: {} paths reported as not part of BIDS: {not_included:?}",
        not_included.len()
    );
}

/// The `fs_stats` reader, end to end over a generated tree: a `.stats` file this crate wrote is
/// one a reader it shares no code with can parse into typed rows.
///
/// The header is the part that could silently fail. `fs_stats` matches columns **by name** off
/// the `# ColHeaders` line, so a generated header that named the columns anything else would
/// parse, ingest, and produce rows that are entirely NULL — which is why this asserts a value
/// and not just a count.
#[tokio::test]
async fn a_generated_stats_file_is_read_into_typed_rows() {
    let dir = tempfile::tempdir().expect("tempdir");
    let schema = schema_with(&["freesurfer"]);
    let layout = bids_schema::layout::bundled_layout("freesurfer").expect("bundled layout");
    Plan::producer(Producer::Layout(Box::new(layout)))
        .scale(small())
        .write(dir.path(), &schema)
        .expect("writes");

    let db = ingest(dir.path(), &schema, &["freesurfer"]).await;

    let (rows, typed): (i64, i64) = db
        .conn
        .query_row(
            "SELECT COUNT(*), COUNT(Volume_mm3) FROM freesurfer_aseg WHERE seg = 'aseg'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("query");
    assert_eq!(
        (rows > 0, rows),
        (true, typed),
        "rows vs non-null Volume_mm3"
    );
}

/// The benchmark's premise, asserted rather than assumed: a generated confounds file reaches the
/// overlay's typed table, and it has exactly one row per volume. Both halves matter — a file that
/// routed nowhere would still leave the ingest looking fast.
#[tokio::test]
async fn a_generated_confounds_file_routes_to_the_typed_table() {
    let dir = tempfile::tempdir().expect("tempdir");
    let schema = schema_with(&["fmriprep"]);
    let scale = small();
    let expected = (scale.subjects * scale.sessions * scale.runs * scale.confound_rows) as i64;
    Plan::producer(Producer::Fmriprep)
        .scale(scale)
        .write(dir.path(), &schema)
        .expect("writes");

    let db = ingest(dir.path(), &schema, &["fmriprep"]).await;

    let rows: i64 = db
        .conn
        .query_row("SELECT COUNT(*) FROM fmriprep_confounds", [], |r| r.get(0))
        .expect("query");
    assert_eq!(rows, expected);
}

/// Every file the manifest says was written is a file the walk sees, and vice versa.
///
/// The failure this rules out is silent in every other check: a generator that writes a path the
/// walker skips — a dotfile, something a `.bidsignore` covers — produces a tree that looks right
/// on disk and is smaller than intended in the catalog, and a benchmark over it measures a
/// dataset nobody asked for.
#[rstest::rstest]
#[case::raw("raw")]
#[case::fmriprep("fmriprep")]
#[case::freesurfer("freesurfer")]
#[case::feat("feat")]
fn the_generated_tree_registers_every_walked_file(#[case] name: &str) {
    let dir = tempfile::tempdir().expect("tempdir");
    let (producer, adapters) = match name {
        "raw" => (Producer::Raw, vec![]),
        "fmriprep" => (Producer::Fmriprep, vec!["fmriprep"]),
        other => (
            Producer::Layout(Box::new(
                bids_schema::layout::bundled_layout(other).expect("bundled layout"),
            )),
            vec![other],
        ),
    };
    let schema = schema_with(&adapters);
    let manifest = Plan::producer(producer)
        .scale(small())
        .write(dir.path(), &schema)
        .expect("writes");

    let walked: BTreeSet<String> = walk(dir.path());

    let planned: BTreeSet<String> = manifest.files.iter().map(|f| f.rel_path.clone()).collect();
    assert_eq!(walked, planned, "{name}");
}

/// The authoring path, end to end: a layout that is not bundled, loaded from a path, generates
/// its own tree. Everything a producer under development needs, with no code in this crate that
/// knows about it.
#[test]
fn an_unbundled_layout_generates_from_its_own_path() {
    let dir = tempfile::tempdir().expect("tempdir");
    let document = serde_json::json!({
        "LayoutVersion": "0.1.0",
        // A term map still has to be bundled — ADR 0002 resolves one by name only — so this
        // borrows FreeSurfer's while declaring a tree of its own.
        "TermMap": "freesurfer",
        "Roles": {
            "aseg_stats": {
                "Template": "stats/aseg.stats",
                "Concepts": { "datatype": "anat", "suffix": "segstats" },
                "Entities": { "seg": "aseg" },
                "Description": "The one role this experimental layout declares."
            }
        },
        "Examples": [{ "Root": "sub-01_ses-V1" }]
    });
    let path = dir.path().join("experimental.json");
    std::fs::write(
        &path,
        serde_json::to_vec_pretty(&document).expect("serializes"),
    )
    .expect("writes");
    let layout = bids_schema::layout::load_layout(&path).expect("layout loads");
    let schema = schema_with(&["freesurfer"]);

    let files = Plan::producer(Producer::Layout(Box::new(layout)))
        .scale(small())
        .paths(&schema)
        .expect("plans");

    assert_eq!(
        files
            .iter()
            .map(|f| f.rel_path.as_str())
            .collect::<Vec<_>>(),
        [
            "sub-0001_ses-V1/stats/aseg.stats",
            "sub-0002_ses-V1/stats/aseg.stats"
        ]
    );
}

/// A benchmark tree has to be the same tree every time, or two criterion runs are not comparable.
/// Nothing here is seeded because nothing here is random; this is what says so.
#[test]
fn two_runs_of_one_plan_write_identical_bytes() {
    let schema = schema_with(&["fmriprep"]);
    let bodies = |_: ()| -> Vec<(String, Option<Vec<u8>>)> {
        Plan::producer(Producer::Fmriprep)
            .scale(small())
            .paths(&schema)
            .expect("plans")
            .into_iter()
            .map(|f| (f.rel_path, f.body.map(|b| b.to_vec())))
            .collect()
    };

    let first = bodies(());

    assert_eq!(first, bodies(()));
}

/// Every claim the generator makes about its own output holds against the schema and term maps
/// that will read it.
#[rstest::rstest]
#[case::raw("raw")]
#[case::fmriprep("fmriprep")]
#[case::freesurfer("freesurfer")]
#[case::feat("feat")]
fn every_claim_verifies(#[case] name: &str) {
    let dir = tempfile::tempdir().expect("tempdir");
    let (producer, adapters) = match name {
        "raw" => (Producer::Raw, vec![]),
        "fmriprep" => (Producer::Fmriprep, vec!["fmriprep"]),
        other => (
            Producer::Layout(Box::new(
                bids_schema::layout::bundled_layout(other).expect("bundled layout"),
            )),
            vec![other],
        ),
    };
    let schema = schema_with(&adapters);
    let term_maps: Vec<_> = adapters
        .iter()
        .filter_map(|a| bids_schema::term_map::bundled_term_map(a))
        .collect();
    let manifest = Plan::producer(producer)
        .scale(small())
        .write(dir.path(), &schema)
        .expect("writes");

    let verified = manifest.verify(&schema, &term_maps);

    assert_eq!(verified, Ok(()), "{name}");
}

/// A tree with no unclaimed files would mean the generator only writes what the schema already
/// knows, which is not what a real producer does: `scripts/`, `touch/` and FIX's scratch are
/// recognized and deliberately claim nothing.
#[test]
fn a_layout_tree_carries_the_bookkeeping_files_that_claim_nothing() {
    let schema = schema_with(&["freesurfer"]);
    let layout = bids_schema::layout::bundled_layout("freesurfer").expect("bundled layout");

    let files = Plan::producer(Producer::Layout(Box::new(layout)))
        .scale(small())
        .paths(&schema)
        .expect("plans");

    let bookkeeping: Vec<&str> = files
        .iter()
        .filter(|f| f.rel_path.contains("/scripts/") || f.rel_path.contains("/touch/"))
        .map(|f| f.rel_path.as_str())
        .collect();
    assert!(!bookkeeping.is_empty(), "no bookkeeping files were planned");
}

/// Ingest, exactly as `bidslake index --adapter …` does.
///
/// `with_term_maps` is not optional and is easy to forget: a `Schema` built with term maps only
/// *knows* about the projection — it shapes the DDL — while the parser is what applies one. Omit
/// it and a FreeSurfer tree walks fine, registers nothing projected, reads no `.stats`, and every
/// count comes back zero with no error anywhere.
async fn ingest(root: &std::path::Path, schema: &Schema, adapters: &[&str]) -> BidsDb {
    let db = BidsDb::new(":memory:").expect("open db");
    db.create_tables(schema).expect("create tables");
    let term_maps: Vec<_> = adapters
        .iter()
        .filter_map(|a| bids_schema::term_map::bundled_term_map(a))
        .collect();
    let fs = Box::new(LocalFileSystem::new(root.to_path_buf()));
    let mut parser =
        BidsParser::new(fs, None, schema.clone(), None, true, true).with_term_maps(term_maps);
    let txn = db.conn.unchecked_transaction().expect("txn");
    parser.parse(&db).await.expect("parse");
    txn.commit().expect("commit");
    db
}

/// Every file the ingest walk would see, dataset-relative — the same walker ingest uses, so the
/// expected set cannot drift from what ingest actually reaches.
fn walk(root: &std::path::Path) -> BTreeSet<String> {
    let schema: serde_json::Value =
        serde_json::from_str(bids_schema::SCHEMA_JSON).expect("schema parses");
    let pseudo = bids_schema::pseudo_file_extensions(&schema);
    let tree = bids_core::filetree::read_file_tree(root, &pseudo, true).expect("walks");
    tree.walk_files()
        .map(|f| f.path.trim_start_matches('/').to_string())
        .collect()
}
