//! HED (Hierarchical Event Descriptors) validation.
//!
//! Unlike the other `rules.errors` checks, HED validation does not fit the
//! one-validator/one-boolean [`super::ErrorValidator`] shape: a single events file produces
//! many issues, each with its own message/subcode, that map onto the six coarse `HED_*`
//! BIDS codes. So HED is handled here by a dedicated per-file pass, [`check_hed_file`],
//! invoked from the validator's file walk. The heavy lifting is delegated to the
//! `hed-validator-rs` engine; this module only detects applicability, feeds it the merged
//! sidecar + event table, and maps the returned [`HedError`]s onto BIDS issues.
//!
//! [`HED_ERROR_KEYS`] lists the schema keys handled here; `test_all_schema_errors_implemented`
//! treats them as implemented by this module rather than the [`super::VALIDATORS`] registry.

use crate::context::{BidsContext, DatasetContext};
use crate::issues::{BidsIssue, DatasetIssues};
use crate::schema::BidsSchema;
use hed_validator_rs::data::{HedColumnDef, Sidecar, TabularInput};
use hed_validator_rs::errors::HedError;
use hed_validator_rs::parser::parse_hed_string;
use hed_validator_rs::schema::SchemaCollection;
use hed_validator_rs::validator::{
    DefinitionMap, DefinitionSite, PlaceholderMode, ValidationContext, Validator,
    gather_definitions, sidecar_validator, tabular_validator,
};
use serde_json::Value;

/// The `rules.errors` keys handled by this module instead of the generic `ErrorValidator`
/// registry.
pub const HED_ERROR_KEYS: &[&str] = &[
    "HedError",
    "HedWarning",
    "HedInternalError",
    "HedInternalWarning",
    "HedMissingValueInSidecar",
    "HedVersionNotDefined",
];

/// Run HED validation for a single file and add any resulting issues.
///
/// Applies only to events `.tsv` files that carry HED data (a literal `HED` column or a
/// sidecar with HED annotations), matching the schema selectors for the `HED_*` codes and
/// the TypeScript validator's `hed.ts`.
pub async fn check_hed_file(
    context: &BidsContext,
    dataset: &DatasetContext,
    schema: &BidsSchema,
    issues: &mut DatasetIssues,
) {
    // Applicability: an events .tsv with HED data.
    let is_events_tsv = context.suffix == "events" && context.extension == ".tsv";
    let has_hed = context.columns.contains_key("HED") || sidecar_has_hed(&context.sidecar);
    if !is_events_tsv || !has_hed {
        return;
    }

    // A HED version must be declared to validate.
    let Some(schemas) = dataset.hed_schemas.as_ref() else {
        match &dataset.hed_schema_error {
            // HEDVersion declared but the schema build failed.
            Some(err) => add_issue(issues, context, schema, "HedInternalError", Some(err)),
            // No HEDVersion declared at all.
            None => add_issue(issues, context, schema, "HedVersionNotDefined", None),
        }
        return;
    };

    let hed_errors = validate_events(schemas, &context.sidecar, context);
    for hed in &hed_errors {
        let key = map_key(hed);
        add_issue(issues, context, schema, key, Some(&format_detail(hed)));
    }
}

/// True if any top-level sidecar value is an object with a defined `HED` key (port of the TS
/// `sidecarHasHed`).
fn sidecar_has_hed(sidecar: &Value) -> bool {
    sidecar
        .as_object()
        .map(|obj| {
            obj.values()
                .any(|v| v.as_object().is_some_and(|m| m.contains_key("HED")))
        })
        .unwrap_or(false)
}

/// Validate an events table together with its merged sidecar, returning all HED issues.
///
/// Mirrors `validate_sidecar_and_gather` + `run_combo_case` from the `hed-validator-rs` test
/// harness: validate the sidecar shape/braces/placeholders, gather every definition declared
/// anywhere in the sidecar into one map, validate the standalone (non-spliced) column strings,
/// then run the assembled per-row tabular validation with those definitions.
fn validate_events(
    schemas: &SchemaCollection,
    sidecar_json: &Value,
    context: &BidsContext,
) -> Vec<HedError> {
    let mut errors = Vec::new();

    errors.extend(sidecar_validator::validate_sidecar_shape(sidecar_json));
    sidecar_validator::validate_braces(sidecar_json, &mut errors);

    let Ok(sidecar) = Sidecar::parse(sidecar_json) else {
        return errors;
    };
    sidecar_validator::validate_placeholder_counts(&sidecar.columns, &mut errors);

    // Gather all definitions across the whole sidecar before validating any Def usage, then
    // validate each column that isn't purely a `{col}` splice source.
    let mut defs = DefinitionMap::new();
    let referenced = tabular_validator::statically_referenced_columns(&sidecar.columns);
    let mut strings_by_mode: Vec<(String, PlaceholderMode)> = Vec::new();
    for (col_name, def) in &sidecar.columns {
        if referenced.contains(col_name) {
            continue;
        }
        match def {
            HedColumnDef::Value(s) => {
                strings_by_mode.push((s.clone(), PlaceholderMode::ValueColumn));
            }
            HedColumnDef::Categorical(map) => {
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

    // Assembled per-row tabular validation.
    if let Some(tabular) = build_tabular(context) {
        errors.extend(tabular_validator::validate_tabular(
            schemas,
            &tabular,
            &sidecar.columns,
            &defs,
        ));
    }

    errors
}

/// Build a [`TabularInput`] (row-major, header row first) from the context's column-major TSV.
fn build_tabular(context: &BidsContext) -> Option<TabularInput> {
    if context.columns.is_empty() {
        return None;
    }
    let headers: Vec<String> = context.columns.keys().cloned().collect();
    let n_rows = context.columns.values().map(|v| v.len()).max().unwrap_or(0);

    let mut data: Vec<Vec<Value>> = Vec::with_capacity(n_rows + 1);
    data.push(headers.iter().map(|h| Value::String(h.clone())).collect());
    for i in 0..n_rows {
        let row: Vec<Value> = headers
            .iter()
            .map(|h| {
                let cell = context.columns[h]
                    .get(i)
                    .cloned()
                    .unwrap_or_else(|| "n/a".to_string());
                Value::String(cell)
            })
            .collect();
        data.push(row);
    }
    TabularInput::parse(&data).ok()
}

/// Choose the BIDS schema key for a HED issue.
fn map_key(hed: &HedError) -> &'static str {
    if hed.issue_type == "WARNING" {
        if hed.issue_code == "SIDECAR_KEY_MISSING" {
            "HedMissingValueInSidecar"
        } else {
            "HedWarning"
        }
    } else {
        "HedError"
    }
}

/// Render the rich HED detail (code, subcode, message, offending text) for `sub_message`.
fn format_detail(hed: &HedError) -> String {
    let mut detail = if hed.issue_subcode.is_empty() {
        format!("{}: {}", hed.issue_code, hed.message)
    } else {
        format!(
            "{} ({}): {}",
            hed.issue_code, hed.issue_subcode, hed.message
        )
    };
    if let Some(loc) = &hed.location {
        detail.push_str(&format!(" [at: {}]", loc));
    }
    detail
}

/// Add a BIDS issue for the given schema key, using the schema-defined code/message/severity.
fn add_issue(
    issues: &mut DatasetIssues,
    context: &BidsContext,
    schema: &BidsSchema,
    key: &str,
    sub_message: Option<&str>,
) {
    if let Some(rule) = schema.error_rules.get(key) {
        issues.add(BidsIssue {
            code: rule.code.clone(),
            sub_code: None,
            message: rule.message.clone(),
            severity: rule.level,
            location: context.path.clone(),
            rule: None,
            sub_message: sub_message.map(String::from),
        });
    }
}
