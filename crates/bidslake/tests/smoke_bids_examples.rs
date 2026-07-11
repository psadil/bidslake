//! Broad smoke test over the entire `bids-examples` corpus.
//!
//! Ingests every dataset in the submodule and asserts a few invariants that
//! must hold for *all* of them. One dataset failing does not stop the others —
//! failures are collected and reported together so the suite names every
//! offending dataset in a single run.

mod common;

use common::{all_datasets, count};

/// Ingests every dataset in the corpus, so it takes minutes. Excluded from the
/// default `cargo test` to keep iteration fast; run it explicitly with
/// `cargo test --test smoke_bids_examples -- --ignored`, or in CI. The curated
/// tests cover the common paths deeply on every run.
#[ignore = "comprehensive: ingests the whole corpus; run with --ignored"]
#[tokio::test]
async fn ingest_every_bids_example() {
    let datasets = all_datasets();
    println!("Smoke-testing {} bids-examples datasets", datasets.len());

    let mut failures: Vec<String> = Vec::new();

    for (name, path) in &datasets {
        match check_dataset(name, path).await {
            Ok(()) => {}
            Err(e) => failures.push(format!("  {name}: {e}")),
        }
    }

    assert!(
        failures.is_empty(),
        "{} / {} datasets failed to ingest cleanly:\n{}",
        failures.len(),
        datasets.len(),
        failures.join("\n")
    );
}

/// Invariants that must hold for any successfully-ingested dataset.
async fn check_dataset(name: &str, path: &std::path::Path) -> anyhow::Result<()> {
    let db = common::ingest(path)
        .await
        .map_err(|e| anyhow::anyhow!("parse failed: {e}"))?;

    // Every dataset has a dataset_description.json, so it must produce exactly
    // one dataset_description row.
    let desc = count(&db, "dataset_description")?;
    anyhow::ensure!(
        desc == 1,
        "expected 1 dataset_description row, found {desc}"
    );

    // If the dataset ships a participants.tsv, the participants table must be
    // populated (either from the TSV or implicit sub- entities).
    if path.join("participants.tsv").is_file() {
        let participants = count(&db, "participants")?;
        anyhow::ensure!(
            participants > 0,
            "participants.tsv present but participants table is empty"
        );
    }

    let _ = name;
    Ok(())
}
