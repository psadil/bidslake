//! Generates one integration test per dataset found in the `tests/data/bids-examples`
//! submodule. The generated file is `include!`d by `tests/integration_test.rs`, which
//! provides the `bids_example_test!` macro and the `run_example` helper.
//!
//! If the submodule is not checked out (no dataset directories are found), a single
//! placeholder test is generated that prints how to initialize it and passes.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

/// Default BIDS schema version to vendor at build time. Matches the `@bids/schema` version the
/// reference TS validator pins, so the two validators use an identical default schema. Override
/// with the `BIDS_SCHEMA_VERSION` environment variable at build time.
const DEFAULT_SCHEMA_VERSION: &str = "1.2.4";

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());

    vendor_schema(&manifest, &out_dir);
    generate_example_tests(&manifest, &out_dir);
    generate_expression_tests(&out_dir);
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

/// Vendor the BIDS schema into `OUT_DIR/schema.json`, which the crate embeds via `include_str!`.
///
/// Prefers the committed copy at `vendor/bids-schema.json` (pinned to
/// [`DEFAULT_SCHEMA_VERSION`], the `@bids/schema` version the reference TS validator bundles),
/// so a clean build needs **no network**. Only an explicit `BIDS_SCHEMA_VERSION` that differs
/// from the committed version triggers a download from jsr.io — the workflow for bumping the
/// pinned schema (fetch the new version, then overwrite `vendor/bids-schema.json`).
fn vendor_schema(manifest: &Path, out_dir: &Path) {
    println!("cargo:rerun-if-env-changed=BIDS_SCHEMA_VERSION");
    println!("cargo:rerun-if-changed=vendor/bids-schema.json");

    let vendored = manifest.join("vendor/bids-schema.json");
    let requested = env::var("BIDS_SCHEMA_VERSION").ok();

    // The committed schema is DEFAULT_SCHEMA_VERSION. Reach for the network only when an explicit
    // *different* version is requested; otherwise use the offline copy.
    let body = match requested {
        Some(version) if version != DEFAULT_SCHEMA_VERSION => {
            let url = format!("https://jsr.io/@bids/schema/{version}/schema.json");
            let resp = ureq::get(&url)
                .call()
                .unwrap_or_else(|e| panic!("failed to download BIDS schema from {url}: {e}"));
            resp.into_string()
                .unwrap_or_else(|e| panic!("failed to read BIDS schema response from {url}: {e}"))
        }
        _ => fs::read_to_string(&vendored).unwrap_or_else(|e| {
            panic!(
                "failed to read committed BIDS schema {}: {e}",
                vendored.display()
            )
        }),
    };

    assert!(
        body.trim_start().starts_with('{'),
        "unexpected (non-JSON) BIDS schema content"
    );
    fs::write(out_dir.join("schema.json"), body).expect("write schema.json to OUT_DIR");
}

fn generate_example_tests(manifest: &Path, out_dir: &Path) {
    let examples = manifest.join("tests/data/bids-examples");
    let dest = out_dir.join("bids_examples_tests.rs");

    // Regenerate when the set of example datasets (or this script) changes.
    println!("cargo:rerun-if-changed=tests/data/bids-examples");
    println!("cargo:rerun-if-changed=build.rs");

    let mut datasets: Vec<String> = Vec::new();
    if let Ok(entries) = fs::read_dir(&examples) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|n| n.to_str()).map(String::from) else {
                continue;
            };
            // Skip hidden dirs, the repo's own docs/tools, and datasets flagged to skip.
            if name.starts_with('.') || name == "docs" || name == "tools" {
                continue;
            }
            if path.join(".SKIP_VALIDATION").exists() {
                continue;
            }
            datasets.push(name);
        }
    }
    datasets.sort();

    let mut out = String::new();
    if datasets.is_empty() {
        out.push_str(concat!(
            "#[test]\n",
            "fn bids_examples_submodule_missing() {\n",
            "    eprintln!(\"bids-examples submodule not checked out; no example datasets found. Run: git submodule update --init tests/data/bids-examples\");\n",
            "}\n",
        ));
    } else {
        for name in &datasets {
            let ident = sanitize(name);
            out.push_str(&format!("bids_example_test!({ident}, \"{name}\");\n"));
        }
    }

    fs::write(&dest, out).unwrap();
}

/// Turn a dataset directory name into a valid, unique Rust identifier.
fn sanitize(name: &str) -> String {
    let mut ident = String::from("example_");
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            ident.push(c);
        } else {
            ident.push('_');
        }
    }
    ident
}
