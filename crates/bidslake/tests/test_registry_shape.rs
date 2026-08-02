//! Datasets accumulate in one catalog across `index` runs (ADR 0002 §3), but tables are
//! created `IF NOT EXISTS` — so the file registry keeps the shape of whichever run created
//! it. A later run whose overlays or term maps are wider has nowhere to record the
//! difference, and would drop it without saying so.
//!
//! These pin the guard that turns that silence into an error, and the property that makes
//! the remedy work: the adapter set describes the *catalog*, so passing all of them on
//! every run gives the same registry whatever the order.

use bidslake::db::BidsDb;
use bidslake::schema::{AppliedOverlay, Ingestion, Schema};

/// Build a schema the way `bidslake index --adapter <names...>` would.
fn schema_for(adapters: &[&str]) -> anyhow::Result<Schema> {
    let mut overlays: Vec<AppliedOverlay> = Vec::new();
    let mut term_maps = Vec::new();
    let mut sources: Vec<String> = vec![
        bids_schema::bundled_ingestion_source("base")
            .expect("base ingestion")
            .to_string(),
    ];
    for name in adapters {
        if let Some(o) = bids_schema::overlay::bundled_overlay(name) {
            overlays.push(AppliedOverlay {
                source: (*name).to_string(),
                content: o,
            });
        }
        if let Some(tm) = bids_schema::term_map::bundled_term_map(name) {
            term_maps.push(tm);
        }
        if let Some(i) = bids_schema::bundled_ingestion_source(name) {
            sources.push(i.to_string());
        }
    }
    let ingestion =
        Ingestion::from_sources(&sources.iter().map(String::as_str).collect::<Vec<_>>())?;
    Schema::load_full(None, &overlays, ingestion, &term_maps)
}

fn registry_columns(db: &BidsDb) -> anyhow::Result<Vec<String>> {
    let mut stmt = db
        .conn
        .prepare("SELECT column_name FROM information_schema.columns WHERE table_name = 'scans'")?;
    let cols = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(cols)
}

/// fMRIPrep's overlay adds `from`/`to`/`mode`; FreeSurfer's does not. Creating the
/// registry from the narrower one and then indexing the wider one must not proceed
/// quietly — those entities would be absent from every row with nothing to show for it.
#[test]
fn widening_an_existing_registry_is_refused() -> anyhow::Result<()> {
    let db = BidsDb::new(":memory:")?;
    db.create_tables(&schema_for(&["freesurfer"])?)?;

    let err = db
        .create_tables(&schema_for(&["fmriprep"])?)
        .expect_err("indexing a wider adapter into a narrower catalog must fail");
    let msg = err.to_string();
    for want in ["from", "to", "mode", "adapter set describes the catalog"] {
        assert!(msg.contains(want), "message should mention {want:?}: {msg}");
    }
    Ok(())
}

/// The projection column is subject to the same rule: a term map has nowhere to store
/// what it computed if the registry was created without one.
#[test]
fn adding_a_term_map_to_a_projection_free_registry_is_refused() -> anyhow::Result<()> {
    let db = BidsDb::new(":memory:")?;
    db.create_tables(&Schema::load(None)?)?; // plain BIDS: no `projected` column

    let err = db
        .create_tables(&schema_for(&["freesurfer"])?)
        .expect_err("a term map needs a projection column that this registry lacks");
    assert!(
        err.to_string().contains("projected"),
        "message should name the missing column: {err}"
    );
    Ok(())
}

/// The remedy, and the reason it is the right one: naming every adapter the catalog uses
/// produces one registry shape, so run order stops mattering.
#[test]
fn passing_every_adapter_makes_run_order_irrelevant() -> anyhow::Result<()> {
    let mut shapes = Vec::new();
    for order in [["fmriprep", "freesurfer"], ["freesurfer", "fmriprep"]] {
        let db = BidsDb::new(":memory:")?;
        // Both runs name both adapters -- what the error message tells users to do.
        db.create_tables(&schema_for(&order)?)?;
        db.create_tables(&schema_for(&order)?)?;
        let mut cols = registry_columns(&db)?;
        cols.sort();
        shapes.push(cols);
    }
    assert_eq!(
        shapes[0], shapes[1],
        "registry shape must not depend on order"
    );
    for want in ["from", "to", "mode", "seg", "projected"] {
        assert!(
            shapes[0].iter().any(|c| c == want),
            "combined registry should carry {want:?}"
        );
    }
    Ok(())
}

/// Re-indexing with the *same* adapters is the common case and must stay a no-op.
#[test]
fn reindexing_with_the_same_adapters_is_allowed() -> anyhow::Result<()> {
    let db = BidsDb::new(":memory:")?;
    db.create_tables(&schema_for(&["freesurfer"])?)?;
    db.create_tables(&schema_for(&["freesurfer"])?)?;
    Ok(())
}

/// A catalog built with more than it needs is fine: only a *missing* column is a problem.
#[test]
fn narrowing_is_allowed() -> anyhow::Result<()> {
    let db = BidsDb::new(":memory:")?;
    db.create_tables(&schema_for(&["fmriprep", "freesurfer"])?)?;
    db.create_tables(&Schema::load(None)?)?;
    Ok(())
}
