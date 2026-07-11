//! Enforces the invariant: **every tabular file the schema describes has its
//! contents in the database, and nothing is silently dropped.**
//!
//! For every dataset in the corpus this checks two things against the
//! `tabular_files` provenance table:
//!
//! 1. Every tabular file ingest sees (a `.tsv`/`.tsv.gz`, not a dotfile, not
//!    `.bidsignore`d) is *recorded* — ingested, left on disk, or explicitly
//!    skipped. A file that reaches none of those would be silently dropped.
//! 2. A file is only ever `skipped` when the BIDS schema genuinely does not
//!    describe its suffix. A schema-known file must be `ingested` or (for
//!    compressed recordings, by size policy) `on_disk` — never `skipped`. If a
//!    `_channels.tsv` (or any schema-known suffix) shows up skipped, routing has
//!    regressed.
//!
//! The check is on *classification*, not row count: the corpus has empty
//! placeholder files (git-annex pointers, zero-byte `.tsv`) that legitimately
//! contribute zero rows, so a row-count assertion would be wrong.

mod common;

use common::{all_datasets, ingest, walk_tabular};
use std::collections::HashSet;

/// Suffixes the schema routes to a table (rule-based, plus the two headerless
/// recording tables `motion`/`stim`). A file with one of these suffixes must never
/// be recorded as skipped.
const SCHEMA_KNOWN_SUFFIXES: &[&str] = &[
    "participants",
    "samples",
    "sessions",
    "scans",
    "events",
    "beh",
    "channels",
    "electrodes",
    "optodes",
    "blood",
    "aslcontext",
    "physio",
    "physioevents",
    "stim",
    "motion",
    "phenotype",
    "descriptions",
    "dseg",
    "probseg",
];

/// BIDS suffix of a tabular file path: the token after the last `_` before the
/// extension, or the stem for `participants.tsv`/`samples.tsv`.
fn suffix_of(file_path: &str) -> String {
    let name = file_path.rsplit('/').next().unwrap_or(file_path);
    let stem = name
        .strip_suffix(".tsv.gz")
        .or_else(|| name.strip_suffix(".tsv"))
        .unwrap_or(name);
    stem.rsplit('_').next().unwrap_or(stem).to_string()
}

/// Ingests every dataset in the corpus, so it is slow (minutes). Run it
/// explicitly — `cargo test --test tabular_coverage -- --ignored` — or in CI;
/// it is excluded from the default `cargo test` to keep iteration fast.
#[ignore = "comprehensive: ingests the whole corpus; run with --ignored"]
#[tokio::test]
async fn all_tabular_data_is_in_the_database() {
    let mut dropped: Vec<String> = Vec::new();
    let mut wrongly_skipped: Vec<String> = Vec::new();

    for (name, path) in all_datasets() {
        let db = ingest(&path)
            .await
            .unwrap_or_else(|e| panic!("{name}: ingest failed: {e}"));

        // Everything recorded in tabular_files, and the subset marked skipped.
        let recorded: HashSet<String> = db
            .conn
            .prepare("SELECT file_path FROM tabular_files")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        let skipped: Vec<String> = db
            .conn
            .prepare("SELECT file_path FROM tabular_files WHERE status = 'skipped'")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();

        // (1) Every tabular file on disk was recorded.
        for f in walk_tabular(&path) {
            if !recorded.contains(&f) {
                dropped.push(format!("{name}: {f}"));
            }
        }

        // (2) Skipped files have a genuinely unknown suffix.
        for f in &skipped {
            let suffix = suffix_of(f);
            if !SCHEMA_KNOWN_SUFFIXES.contains(&suffix.as_str()) {
                continue;
            }
            // `participants`/`samples` are matched by a root-anchored selector
            // (`path == "/participants.tsv"`); a nested one belongs to a derivative
            // dataset and legitimately does not match, so only flag root files.
            if matches!(suffix.as_str(), "participants" | "samples") && f.contains('/') {
                continue;
            }
            wrongly_skipped.push(format!("{name}: {f} (suffix `{suffix}`)"));
        }
    }

    assert!(
        dropped.is_empty(),
        "tabular files silently dropped (not in tabular_files):\n{}",
        dropped.join("\n")
    );
    assert!(
        wrongly_skipped.is_empty(),
        "schema-known tabular files were skipped instead of ingested:\n{}",
        wrongly_skipped.join("\n")
    );
}
