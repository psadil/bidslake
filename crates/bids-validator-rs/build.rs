//! Generates one integration test per dataset found in the `tests/data/bids-examples`
//! submodule. The generated file is `include!`d by `tests/integration_test.rs`, which
//! provides the `bids_example_test!` macro and the `run_example` helper.
//!
//! If the submodule is not checked out (no dataset directories are found), a single
//! placeholder test is generated that prints how to initialize it and passes.
//!
//! The BIDS schema itself and the expression-language conformance tests are owned by the
//! `bids-schema` crate (`bids_schema::SCHEMA_JSON`); this build script no longer vendors a
//! schema or fetches anything.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());

    generate_example_tests(&manifest, &out_dir);
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
