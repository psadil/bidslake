use crate::data::HedColumnDef;
use crate::errors::{HedError, codes};
use regex::Regex;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

static BRACE_REF: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\{([^{}]*)\}").expect("static brace regex is valid"));

/// A sidecar Value column's template string must contain exactly one literal `#`
/// (Categorical/plain-tag columns must contain none, but that's enforced per-value by the
/// generic tag validator's placeholder-mode check instead).
pub fn validate_placeholder_counts(
    columns: &HashMap<String, HedColumnDef>,
    errors: &mut Vec<HedError>,
) {
    for def in columns.values() {
        if let HedColumnDef::Value(s) = def
            && s.matches('#').count() != 1
        {
            errors.push(HedError::error(
                codes::PLACEHOLDER_INVALID,
                "a sidecar Value column's HED template must contain exactly one '#'",
                Some(s.clone()),
            ));
        }
    }
}

/// Pure JSON-shape checks for a sidecar, independent of `Sidecar::parse` succeeding: the
/// `"HED"` key must sit exactly one level inside each column, and the reserved `"n/a"`
/// category (the BIDS missing-value marker) may not carry an annotation.
pub fn validate_sidecar_shape(json: &Value) -> Vec<HedError> {
    let mut errors = Vec::new();
    let Some(root_obj) = json.as_object() else {
        return errors;
    };

    if root_obj.contains_key("HED") {
        errors.push(HedError::error(
            codes::SIDECAR_INVALID,
            "'HED' must be nested under a column name, not at the sidecar root",
            None,
        ));
    }

    for col_val in root_obj.values() {
        let Some(col_obj) = col_val.as_object() else {
            continue;
        };
        match col_obj.get("HED") {
            Some(Value::Object(categories)) => {
                if categories.contains_key("n/a") {
                    errors.push(HedError::error(
                        codes::SIDECAR_INVALID,
                        "'n/a' is reserved for missing values and cannot have a HED annotation",
                        None,
                    ));
                }
            }
            Some(_) => {}
            None => {
                // No "HED" at the expected level — flag if it's nested one level too deep.
                let nested_too_deep = col_obj
                    .values()
                    .any(|v| v.as_object().is_some_and(|inner| inner.contains_key("HED")));
                if nested_too_deep {
                    errors.push(HedError::error(
                        codes::SIDECAR_INVALID,
                        "'HED' key is nested too deep; it must be a direct child of the column",
                        None,
                    ));
                }
            }
        }
    }

    errors
}

fn extract_brace_refs(s: &str) -> Vec<String> {
    BRACE_REF
        .captures_iter(s)
        .map(|c| c[1].to_string())
        .collect()
}

fn collect_column_strings(json: &Value) -> HashMap<String, Vec<String>> {
    let mut columns = HashMap::new();
    let Some(root_obj) = json.as_object() else {
        return columns;
    };
    for (col_name, col_val) in root_obj {
        let Some(hed_val) = col_val.as_object().and_then(|o| o.get("HED")) else {
            continue;
        };
        let mut strings = Vec::new();
        match hed_val {
            Value::String(s) => strings.push(s.clone()),
            Value::Object(cat) => {
                for v in cat.values() {
                    if let Some(s) = v.as_str() {
                        strings.push(s.to_string());
                    }
                }
            }
            _ => {}
        }
        columns.insert(col_name.clone(), strings);
    }
    columns
}

/// Validates `{col}` splice references across a sidecar: every reference must name the
/// literal sink `HED` or an existing column that itself has a `"HED"` annotation; no column
/// may reference itself; and any column that IS referenced from elsewhere must not itself
/// contain further references (references only ever resolve one hop deep).
pub fn validate_braces(json: &Value, errors: &mut Vec<HedError>) {
    let columns = collect_column_strings(json);

    for (col_name, strings) in &columns {
        for s in strings {
            let refs = extract_brace_refs(s);
            for r in &refs {
                if r != "HED" && !columns.contains_key(r) {
                    errors.push(HedError::error(
                        codes::SIDECAR_BRACES_INVALID,
                        "curly-brace reference does not name 'HED' or an existing HED-annotated column",
                        Some(s.clone()),
                    ));
                }
            }
            if refs.iter().any(|r| r == col_name) {
                errors.push(HedError::error(
                    codes::SIDECAR_BRACES_INVALID,
                    "a column cannot reference itself",
                    Some(s.clone()),
                ));
            }
        }
    }

    let mut referenced: HashSet<String> = HashSet::new();
    for strings in columns.values() {
        for s in strings {
            for r in extract_brace_refs(s) {
                if r != "HED" {
                    referenced.insert(r);
                }
            }
        }
    }

    for target in &referenced {
        if let Some(strings) = columns.get(target) {
            for s in strings {
                if !extract_brace_refs(s).is_empty() {
                    errors.push(HedError::error(
                        codes::SIDECAR_BRACES_INVALID,
                        "a referenced column's annotation cannot itself contain further references",
                        Some(s.clone()),
                    ));
                }
            }
        }
    }
}
