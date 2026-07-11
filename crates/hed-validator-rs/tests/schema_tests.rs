//! Conformance harness for tests/hed-tests/json_test_data/schema_test_data/*.json:
//! each case supplies an inline mediawiki schema source expected to either fail loading /
//! compliance-checking with a specific error code, or to be fully clean.

use hed_validator_rs::schema::{Schema, SchemaLoadError, check_compliance, load_wiki_string};
use serde::Deserialize;
use std::fs;

#[derive(Deserialize, Debug)]
struct SchemaTest {
    error_code: String,
    alt_codes: Option<Vec<String>>,
    name: Option<String>,
    tests: TestGroups,
}

#[derive(Deserialize, Debug, Default)]
struct TestGroups {
    schema_tests: Option<SchemaCases>,
}

#[derive(Deserialize, Debug)]
struct SchemaCases {
    fails: Vec<Vec<String>>,
    passes: Vec<Vec<String>>,
}

/// Base loader for `withStandard` partner schemas: only the embedded 8.4.0 is available
/// here (fixtures that name a nonexistent partner version are exactly the ones expected to
/// fail with SCHEMA_LIBRARY_INVALID).
fn base_loader(version: &str) -> Result<Schema, SchemaLoadError> {
    if version == "8.4.0" {
        Schema::load_standard("8.4.0")
            .map_err(|e| SchemaLoadError::single("SCHEMA_LOAD_FAILED", &e.to_string()))
    } else {
        Err(SchemaLoadError::single(
            "SCHEMA_LOAD_FAILED",
            &format!("standard schema version '{}' is not available", version),
        ))
    }
}

fn issues_for(case: &[String]) -> Vec<String> {
    let text = case.join("\n");
    match load_wiki_string(&text, None, &base_loader) {
        Err(e) => e.issues.iter().map(|i| i.issue_code.clone()).collect(),
        Ok(schema) => check_compliance(&schema)
            .iter()
            .map(|i| i.issue_code.clone())
            .collect(),
    }
}

#[test]
fn test_schema_tests_suite() {
    let mut paths: Vec<_> = glob::glob("tests/hed-tests/json_test_data/schema_test_data/*.json")
        .expect("glob pattern must be valid")
        .filter_map(|e| e.ok())
        .collect();
    paths.sort();
    assert!(
        !paths.is_empty(),
        "expected to find schema_test_data/*.json files"
    );

    for path in paths {
        let json_data = fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("failed to read {:?}: {}", path, e));
        let tests: Vec<SchemaTest> = serde_json::from_str(&json_data)
            .unwrap_or_else(|e| panic!("failed to parse {:?}: {}", path, e));

        for test in tests {
            let mut accepted: Vec<&str> = vec![test.error_code.as_str()];
            if let Some(alts) = &test.alt_codes {
                accepted.extend(alts.iter().map(String::as_str));
            }
            let label = test.name.as_deref().unwrap_or("(unnamed)");

            let Some(cases) = &test.tests.schema_tests else {
                continue;
            };

            for (i, fail_case) in cases.fails.iter().enumerate() {
                let codes = issues_for(fail_case);
                assert!(
                    codes.iter().any(|c| accepted.contains(&c.as_str())),
                    "[{} / {} fail #{}] expected one of {:?}, got {:?}\nschema:\n{}",
                    test.error_code,
                    label,
                    i,
                    accepted,
                    codes,
                    fail_case.join("\n")
                );
            }
            for (i, pass_case) in cases.passes.iter().enumerate() {
                let codes = issues_for(pass_case);
                assert!(
                    codes.is_empty(),
                    "[{} / {} pass #{}] expected no issues, got {:?}\nschema:\n{}",
                    test.error_code,
                    label,
                    i,
                    codes,
                    pass_case.join("\n")
                );
            }
        }
    }
}
