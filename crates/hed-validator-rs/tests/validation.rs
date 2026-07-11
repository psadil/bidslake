use hed_validator_rs::data::{Sidecar, TabularInput};
use hed_validator_rs::errors::HedError;
use hed_validator_rs::parser::parse_hed_string;
use hed_validator_rs::schema::{Schema, SchemaCollection, load_schema_version};
use hed_validator_rs::validator::{
    DefinitionMap, DefinitionSite, PlaceholderMode, ValidationContext, Validator,
    gather_definitions, sidecar_validator, tabular_validator,
};
use serde::Deserialize;
use serde_json::Value;
use std::fs;

#[derive(Deserialize, Debug)]
struct ValidationTest {
    error_code: String,
    alt_codes: Option<Vec<String>>,
    warning: Option<bool>,
    schema: Option<Value>,
    definitions: Option<Vec<String>>,
    tests: TestGroups,
}

#[derive(Deserialize, Debug, Default)]
struct TestGroups {
    string_tests: Option<StringTests>,
    sidecar_tests: Option<StringTests>,
    event_tests: Option<EventTests>,
    combo_tests: Option<ComboTests>,
}

#[derive(Deserialize, Debug)]
struct StringTests {
    fails: Vec<Value>,
    passes: Vec<Value>,
}

#[derive(Deserialize, Debug)]
struct EventTests {
    fails: Vec<Vec<Vec<Value>>>,
    passes: Vec<Vec<Vec<Value>>>,
}

#[derive(Deserialize, Debug)]
struct ComboCase {
    sidecar: Value,
    events: Vec<Vec<Value>>,
}

#[derive(Deserialize, Debug)]
struct ComboTests {
    fails: Vec<ComboCase>,
    passes: Vec<ComboCase>,
}

/// Parses a test entry's `schema` field (a version string or a merge-group array) into a
/// loader spec list. `None` means "use the default embedded 8.4.0".
fn parse_schema_field(schema: &Option<Value>) -> Option<Vec<String>> {
    match schema {
        None => None,
        Some(Value::String(s)) if s.is_empty() || s == "8.4.0" => None,
        Some(Value::String(s)) => Some(vec![s.clone()]),
        Some(Value::Array(items)) => Some(
            items
                .iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect(),
        ),
        Some(other) => panic!("unexpected schema field shape: {other:?}"),
    }
}

fn severity_for(warning: bool) -> &'static str {
    if warning { "WARNING" } else { "ERROR" }
}

fn matches_expected(errors: &[HedError], accepted: &[&str], expected_severity: &str) -> bool {
    errors
        .iter()
        .any(|e| e.issue_type == expected_severity && accepted.contains(&e.issue_code.as_str()))
}

fn has_error_severity(errors: &[HedError]) -> bool {
    errors.iter().any(|e| e.issue_type == "ERROR")
}

fn gather_and_validate(
    schemas: &SchemaCollection,
    text: &str,
    site: DefinitionSite,
    placeholder_mode: PlaceholderMode,
    base_defs: &DefinitionMap,
) -> Vec<HedError> {
    let mut errors = Vec::new();
    let hed_string = match parse_hed_string(text) {
        Ok(s) => s,
        Err(e) => {
            errors.push(e);
            return errors;
        }
    };

    let mut defs = base_defs.clone();
    gather_definitions(schemas, &hed_string.nodes, site, &mut defs, &mut errors);

    let ctx = ValidationContext::new(placeholder_mode, site, &defs);
    let validator = Validator::new(schemas);
    errors.extend(validator.validate(&hed_string, &ctx));
    errors
}

fn run_string_case(
    schemas: &SchemaCollection,
    text: &str,
    site: DefinitionSite,
    placeholder_mode: PlaceholderMode,
    base_defs: &DefinitionMap,
) -> Vec<HedError> {
    gather_and_validate(schemas, text, site, placeholder_mode, base_defs)
}

/// Runs every sidecar-shape/brace/placeholder/definition/tag check on `sidecar_json`,
/// returning both the accumulated errors and the resulting `DefinitionMap` (defs declared
/// anywhere in the sidecar, plus `base_defs`) — `combo_tests` needs that map to also validate
/// `Def`/`Def-expand` usage in the accompanying event rows.
fn validate_sidecar_and_gather(
    schemas: &SchemaCollection,
    sidecar_json: &Value,
    base_defs: &DefinitionMap,
) -> (Vec<HedError>, DefinitionMap) {
    let mut errors = Vec::new();

    errors.extend(sidecar_validator::validate_sidecar_shape(sidecar_json));
    sidecar_validator::validate_braces(sidecar_json, &mut errors);

    let Ok(sidecar) = Sidecar::parse(sidecar_json) else {
        return (errors, base_defs.clone());
    };

    sidecar_validator::validate_placeholder_counts(&sidecar.columns, &mut errors);

    // Definitions are gathered across the whole sidecar (every column, every entry) into one
    // shared map before any Def/Def-expand usage is checked, matching hed-python's
    // whole-sidecar DefinitionDict.
    let mut defs = base_defs.clone();
    let mut strings_by_mode: Vec<(String, PlaceholderMode)> = Vec::new();
    // A column referenced via `{col}` elsewhere in the sidecar is purely a splice source —
    // its template is only ever meant to be substituted into place (where the surrounding
    // context supplies whatever structure it needs, e.g. an enclosing group a temporal tag
    // requires), so it isn't independently validated standalone here.
    let referenced = tabular_validator::statically_referenced_columns(&sidecar.columns);
    for (col_name, def) in &sidecar.columns {
        if referenced.contains(col_name) {
            continue;
        }
        match def {
            hed_validator_rs::data::HedColumnDef::Value(s) => {
                strings_by_mode.push((s.clone(), PlaceholderMode::ValueColumn));
            }
            hed_validator_rs::data::HedColumnDef::Categorical(map) => {
                for s in map.values() {
                    strings_by_mode.push((s.clone(), PlaceholderMode::ForbiddenStrict));
                }
            }
        }
    }

    for (text, _) in &strings_by_mode {
        if let Ok(parsed) = parse_hed_string(text) {
            gather_definitions(
                schemas,
                &parsed.nodes,
                DefinitionSite::SidecarColumn,
                &mut defs,
                &mut errors,
            );
        }
    }

    for (text, mode) in &strings_by_mode {
        match parse_hed_string(text) {
            Ok(parsed) => {
                let ctx = ValidationContext::new(*mode, DefinitionSite::SidecarColumn, &defs);
                let validator = Validator::new(schemas);
                errors.extend(validator.validate(&parsed, &ctx));
            }
            Err(e) => errors.push(e),
        }
    }

    (errors, defs)
}

fn run_sidecar_case(
    schemas: &SchemaCollection,
    sidecar_json: &Value,
    base_defs: &DefinitionMap,
) -> Vec<HedError> {
    validate_sidecar_and_gather(schemas, sidecar_json, base_defs).0
}

/// Runs a bare event table (no sidecar — `onset`/`duration` are the only recognized columns,
/// both pure numeric timing metadata) through tabular + cross-row temporal validation.
fn run_event_case(
    schemas: &SchemaCollection,
    rows: &[Vec<Value>],
    base_defs: &DefinitionMap,
) -> Vec<HedError> {
    let Ok(tabular) = TabularInput::parse(rows) else {
        return Vec::new();
    };
    let columns = std::collections::HashMap::new();
    tabular_validator::validate_tabular(schemas, &tabular, &columns, base_defs)
}

/// Runs a sidecar + event table pair through sidecar validation, then tabular + cross-row
/// temporal validation using the definitions declared in that sidecar.
fn run_combo_case(
    schemas: &SchemaCollection,
    case: &ComboCase,
    base_defs: &DefinitionMap,
) -> Vec<HedError> {
    let (mut errors, defs) = validate_sidecar_and_gather(schemas, &case.sidecar, base_defs);

    let columns = Sidecar::parse(&case.sidecar)
        .map(|s| s.columns)
        .unwrap_or_default();
    let Ok(tabular) = TabularInput::parse(&case.events) else {
        return errors;
    };
    errors.extend(tabular_validator::validate_tabular(
        schemas, &tabular, &columns, &defs,
    ));
    errors
}

fn run_validation_test_file(default_schemas: &SchemaCollection, path: &std::path::Path) {
    let json_data =
        fs::read_to_string(path).unwrap_or_else(|e| panic!("failed to read {:?}: {}", path, e));
    let tests: Vec<ValidationTest> = serde_json::from_str(&json_data)
        .unwrap_or_else(|e| panic!("failed to parse {:?}: {}", path, e));

    for test in tests {
        let is_warning = test.warning.unwrap_or(false);
        let expected_severity = severity_for(is_warning);
        let mut accepted: Vec<&str> = vec![test.error_code.as_str()];
        if let Some(alts) = &test.alt_codes {
            accepted.extend(alts.iter().map(String::as_str));
        }

        // Load the entry's schema (merge groups included). Mirroring hed-python's spec
        // harness: a load failure counts as this entry's expected "fails" outcome — the
        // failure codes must intersect the accepted set, and the entry's sections are
        // then skipped entirely.
        let loaded;
        let schemas: &SchemaCollection = match parse_schema_field(&test.schema) {
            None => default_schemas,
            Some(spec) => {
                match load_schema_version(&spec, Some(std::path::Path::new("tests/hed-schemas"))) {
                    Ok(collection) => {
                        loaded = collection;
                        &loaded
                    }
                    Err(e) => {
                        assert!(
                            e.issues
                                .iter()
                                .any(|i| accepted.contains(&i.issue_code.as_str())),
                            "[{}] schema {:?} failed to load with unexpected codes: {}",
                            test.error_code,
                            spec,
                            e
                        );
                        continue;
                    }
                }
            }
        };

        let mut base_defs = DefinitionMap::new();
        for def_str in test.definitions.iter().flatten() {
            let parsed = parse_hed_string(def_str).unwrap_or_else(|e| {
                panic!("fixture definition failed to parse: {} ({:?})", def_str, e)
            });
            let mut gather_errors = Vec::new();
            gather_definitions(
                schemas,
                &parsed.nodes,
                DefinitionSite::SidecarColumn,
                &mut base_defs,
                &mut gather_errors,
            );
            assert!(
                gather_errors.is_empty(),
                "fixture definition '{}' in {:?} should be well-formed, got {:?}",
                def_str,
                path,
                gather_errors
            );
        }

        if let Some(st) = &test.tests.string_tests {
            for fail_case in &st.fails {
                let Some(s) = fail_case.as_str() else {
                    continue;
                };
                let errors = run_string_case(
                    schemas,
                    s,
                    DefinitionSite::PlainString,
                    PlaceholderMode::Forbidden,
                    &base_defs,
                );
                assert!(
                    matches_expected(&errors, &accepted, expected_severity),
                    "[{}] expected {} for fail case '{}', got {:?}",
                    test.error_code,
                    expected_severity,
                    s,
                    errors
                );
            }
            for pass_case in &st.passes {
                let Some(s) = pass_case.as_str() else {
                    continue;
                };
                let errors = run_string_case(
                    schemas,
                    s,
                    DefinitionSite::PlainString,
                    PlaceholderMode::Forbidden,
                    &base_defs,
                );
                assert!(
                    !has_error_severity(&errors),
                    "[{}] expected no error for pass case '{}', got {:?}",
                    test.error_code,
                    s,
                    errors
                );
            }
        }

        if let Some(st) = &test.tests.sidecar_tests {
            for fail_case in &st.fails {
                let errors = run_sidecar_case(schemas, fail_case, &base_defs);
                assert!(
                    matches_expected(&errors, &accepted, expected_severity),
                    "[{}] expected {} for sidecar fail case {}, got {:?}",
                    test.error_code,
                    expected_severity,
                    fail_case,
                    errors
                );
            }
            for pass_case in &st.passes {
                let errors = run_sidecar_case(schemas, pass_case, &base_defs);
                assert!(
                    !has_error_severity(&errors),
                    "[{}] expected no error for sidecar pass case {}, got {:?}",
                    test.error_code,
                    pass_case,
                    errors
                );
            }
        }

        if let Some(et) = &test.tests.event_tests {
            for fail_case in &et.fails {
                let errors = run_event_case(schemas, fail_case, &base_defs);
                assert!(
                    matches_expected(&errors, &accepted, expected_severity),
                    "[{}] expected {} for event fail case {:?}, got {:?}",
                    test.error_code,
                    expected_severity,
                    fail_case,
                    errors
                );
            }
            for pass_case in &et.passes {
                let errors = run_event_case(schemas, pass_case, &base_defs);
                assert!(
                    !has_error_severity(&errors),
                    "[{}] expected no error for event pass case {:?}, got {:?}",
                    test.error_code,
                    pass_case,
                    errors
                );
            }
        }

        if let Some(ct) = &test.tests.combo_tests {
            for fail_case in &ct.fails {
                let errors = run_combo_case(schemas, fail_case, &base_defs);
                assert!(
                    matches_expected(&errors, &accepted, expected_severity),
                    "[{}] expected {} for combo fail case {:?}, got {:?}",
                    test.error_code,
                    expected_severity,
                    fail_case,
                    errors
                );
            }
            for pass_case in &ct.passes {
                let errors = run_combo_case(schemas, pass_case, &base_defs);
                assert!(
                    !has_error_severity(&errors),
                    "[{}] expected no error for combo pass case {:?}, got {:?}",
                    test.error_code,
                    pass_case,
                    errors
                );
            }
        }
    }
}

#[test]
fn test_basic_validation() {
    let schemas =
        SchemaCollection::single(Schema::load_standard("8.4.0").expect("Failed to load schema"));
    let defs = DefinitionMap::new();

    let errors = run_string_case(
        &schemas,
        "Event, Action/Think",
        DefinitionSite::PlainString,
        PlaceholderMode::Forbidden,
        &defs,
    );
    assert!(
        !has_error_severity(&errors),
        "Expected no errors, got {:?}",
        errors
    );

    let errors = run_string_case(
        &schemas,
        "Event, InvalidTag123",
        DefinitionSite::PlainString,
        PlaceholderMode::Forbidden,
        &defs,
    );
    assert!(errors.iter().any(|e| e.issue_code == "TAG_INVALID"));
}

#[test]
fn test_hed_tests_suite() {
    let schemas =
        SchemaCollection::single(Schema::load_standard("8.4.0").expect("Failed to load schema"));
    let mut paths: Vec<_> =
        glob::glob("tests/hed-tests/json_test_data/validation_test_data/*.json")
            .expect("glob pattern must be valid")
            .filter_map(|e| e.ok())
            .collect();
    paths.sort();

    assert!(
        !paths.is_empty(),
        "expected to find validation_test_data/*.json files"
    );

    for path in paths {
        run_validation_test_file(&schemas, &path);
    }
}
