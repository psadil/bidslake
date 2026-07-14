//! Shared BIDS *semantics*: the raw schema JSON (single source of truth) and the BIDS
//! schema-expression-language evaluator.
//!
//! This crate is the one place that owns the vendored BIDS schema and how to *evaluate* it.
//! (The schema-parameterized association resolver and the shared file-context builder are
//! added in later stages.)
//!
//! NOTE: the *typed* rules struct `BidsSchema` does **not** live here — it stays in
//! `bids_validator_rs::schema::BidsSchema`, built on top of [`SCHEMA_JSON`]. Consumers of this
//! crate reach for `bids_schema::expression`, … and the [`SCHEMA_JSON`] constant.

pub mod associations;
pub mod context;
pub mod datatypes;
pub mod expression;
pub mod overlay;
pub mod term_map;

use serde_json::Value;

/// The BIDS "pseudo-file" extensions (`objects.extensions` values ending in `/`, e.g. `.ds/`,
/// `.mefd/`, `.ome.zarr/`) — directories that BIDS treats as a single opaque file. Pass these to
/// `bids_core::filetree::read_file_tree` so such directories are emitted as files, not descended.
pub fn pseudo_file_extensions(schema: &Value) -> Vec<String> {
    schema
        .get("objects")
        .and_then(|o| o.get("extensions"))
        .and_then(|e| e.as_object())
        .map(|exts| {
            exts.values()
                .filter_map(|v| v.get("value").and_then(|x| x.as_str()))
                .filter(|s| s.ends_with('/'))
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

/// The bundled BIDS schema JSON (schema_version 1.2.1), vendored in-tree from
/// `bids-standard/bids-schema` (pinned; see `third_party/bids-schema/.pinned-commit`) and
/// embedded at build time. The single source of truth for the whole workspace.
pub const SCHEMA_JSON: &str = include_str!(concat!(env!("OUT_DIR"), "/schema.json"));

/// The bundled BIDS **metaschema** — the JSON Schema (draft 2020-12) a valid BIDS
/// schema must satisfy. Vendored (pinned) from `bids-standard/bids-specification` — a
/// *different* upstream repo than [`SCHEMA_JSON`] — and embedded at build time. Used by
/// [`overlay::validate_effective`], which accounts for the metaschema lagging the
/// schema slightly.
pub const METASCHEMA_JSON: &str = include_str!(concat!(env!("OUT_DIR"), "/metaschema.json"));

/// The hand-written JSON-Schema metaschema (draft 2020-12) for bidslake **ingestion
/// schemas** — the bidslake-specific documents that decide read/catalog/ignore + reader +
/// per-table policy for files already projected onto BIDS concepts. Unlike the BIDS
/// metaschema this is bidslake's own (BIDS has no database to read into); it is embedded here
/// only because the bundled data lives in this crate. The [`crate::term_map`] engine is
/// shared; the ingestion *model* lives in `bidslake`.
pub const INGESTION_METASCHEMA_JSON: &str = include_str!("../data/ingestion-metaschema.json");

/// Ingestion fragments bidslake ships, addressable by name.
pub const BUNDLED_INGESTION_NAMES: &[&str] = &["freesurfer"];

/// The raw JSON of a bundled ingestion fragment, or `None` if `name` is not bundled.
pub fn bundled_ingestion_source(name: &str) -> Option<&'static str> {
    Some(match name {
        "base" => include_str!("../data/ingestion/base.json"),
        "freesurfer" => include_str!("../data/ingestion/freesurfer.json"),
        _ => return None,
    })
}

/// Validate an ingestion document against [`INGESTION_METASCHEMA_JSON`], returning the list
/// of violations (empty on success). Lives here because this crate owns the `jsonschema`
/// dependency and the embedded metaschema; the ingestion *model* lives in `bidslake`.
pub fn validate_ingestion(document: &Value) -> Vec<String> {
    let metaschema: Value =
        serde_json::from_str(INGESTION_METASCHEMA_JSON).expect("ingestion metaschema must parse");
    let validator = jsonschema::validator_for(&metaschema)
        .expect("ingestion metaschema must compile as a JSON Schema");
    let mut violations: Vec<String> = validator
        .iter_errors(document)
        .map(|e| format!("  at `{}`: {e}", e.instance_path()))
        .collect();
    violations.sort();
    violations.dedup();
    violations
}
