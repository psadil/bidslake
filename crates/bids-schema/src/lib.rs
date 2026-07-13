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
