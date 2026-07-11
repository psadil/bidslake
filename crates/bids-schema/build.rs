//! Vendors the BIDS schema (from the in-tree `third_party/bids-schema` subtree) into
//! `OUT_DIR/schema.json`, which the crate embeds via `include_str!`, and generates one
//! expression-conformance test per `meta.expression_tests` case.
//!
//! Deterministic and offline: the schema is a committed, in-tree file (a `git subtree` of
//! `bids-standard/bids-schema`), so there is no build-time download and no version branching.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

/// Pinned BIDS spec-version directory within the vendored schema subtree
/// (`third_party/bids-schema/versions/<DIR>/schema.json`). This one is schema_version 1.2.1.
const SCHEMA_VERSION_DIR: &str = "1.11.1";

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());

    vendor_schema(&manifest, &out_dir);
    generate_expression_tests(&out_dir);
}

/// Copy the pinned in-tree `schema.json` into `OUT_DIR` (no network, no fallback).
fn vendor_schema(manifest: &Path, out_dir: &Path) {
    let src = manifest
        .join("../../third_party/bids-schema/versions")
        .join(SCHEMA_VERSION_DIR)
        .join("schema.json");
    println!("cargo:rerun-if-changed={}", src.display());

    let body = fs::read_to_string(&src).unwrap_or_else(|e| {
        panic!(
            "failed to read vendored BIDS schema {} ({e}). Run the bids-schema subtree add/pull.",
            src.display()
        )
    });
    assert!(
        body.trim_start().starts_with('{'),
        "unexpected (non-JSON) BIDS schema content"
    );
    fs::write(out_dir.join("schema.json"), body).expect("write schema.json to OUT_DIR");
}

/// Generate one conformance test per case in the schema's `meta.expression_tests`.
///
/// The BIDS schema ships a normative test suite for its expression language: a list of
/// `{expression, result}` pairs. Emitting one `#[test]` per case (rather than one loop) means a
/// failure names the exact expression. The generated file is `include!`d by
/// `tests/expression_conformance.rs`, which supplies the `expr_test!` macro.
fn generate_expression_tests(out_dir: &Path) {
    let dest = out_dir.join("expression_tests.rs");
    let schema: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(out_dir.join("schema.json")).unwrap())
            .expect("parse vendored schema");

    let cases = schema
        .get("meta")
        .and_then(|m| m.get("expression_tests"))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let mut out = String::new();
    if cases.is_empty() {
        out.push_str("#[test]\nfn expression_tests_missing_from_schema() {\n    panic!(\"meta.expression_tests absent from the vendored schema\");\n}\n");
    } else {
        for (i, case) in cases.iter().enumerate() {
            let expr = case
                .get("expression")
                .and_then(|v| v.as_str())
                .expect("expression_tests entry has no `expression`");
            // `result` may legitimately be absent; treat that as JSON null, matching the YAML.
            let expected = case
                .get("result")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            let ident = format!("expr_{:03}_{}", i, sanitize_expr(expr));
            out.push_str(&format!(
                "expr_test!({ident}, r##\"{expr}\"##, r##\"{expected}\"##);\n"
            ));
        }
    }
    fs::write(&dest, out).unwrap();
}

/// Turn an expression into a short, unique-enough, valid Rust identifier fragment.
fn sanitize_expr(expr: &str) -> String {
    let mut s = String::new();
    for c in expr.chars().take(40) {
        if c.is_ascii_alphanumeric() {
            s.push(c.to_ascii_lowercase());
        } else if !s.ends_with('_') {
            s.push('_');
        }
    }
    s.trim_matches('_').to_string()
}
