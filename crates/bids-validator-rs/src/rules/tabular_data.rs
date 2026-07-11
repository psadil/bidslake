use crate::context::BidsContext;
use crate::expression::{EvalContext, do_selectors_select};
use crate::issues::{BidsIssue, DatasetIssues, Severity};
use crate::rules::RequirementLevel;
use crate::schema::BidsSchema;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Deserialize, Clone)]
pub struct TabularRuleDef {
    pub selectors: Option<Vec<String>>,
    pub columns: HashMap<String, RequirementLevel>,
    pub initial_columns: Option<Vec<String>>,
    pub additional_columns: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
pub enum TabularNode {
    Rule(TabularRuleDef),
    Category(HashMap<String, TabularNode>),
}

/// Apply tabular data rules: check required/optional columns.
pub fn check_tabular_rules(
    context: &BidsContext,
    ctx_value: &EvalContext,
    schema: &BidsSchema,
    issues: &mut DatasetIssues,
) {
    // tsv rules are odd; while extension == '.tsv' is a known selector, the assumption is that
    // that selector is generally not present in these rules.
    if !(context.path.ends_with(".tsv") || context.path.ends_with(".tsv.gz")) {
        return;
    }

    for (category, node) in &schema.tabular_data_rules {
        apply_tabular_node(
            node,
            &format!("rules.tabular_data.{}", category),
            ctx_value,
            context,
            schema,
            issues,
        );
    }
}

fn apply_tabular_node(
    node: &TabularNode,
    path: &str,
    ctx_value: &EvalContext,
    context: &BidsContext,
    schema: &BidsSchema,
    issues: &mut DatasetIssues,
) {
    match node {
        TabularNode::Rule(rule) => {
            eval_tabular_rule(rule, path, ctx_value, context, schema, issues);
            eval_initial_columns(rule, path, ctx_value, context, schema, issues);
        }
        TabularNode::Category(map) => {
            for (key, child) in map {
                apply_tabular_node(
                    child,
                    &format!("{}.{}", path, key),
                    ctx_value,
                    context,
                    schema,
                    issues,
                );
            }
        }
    }
}

/// Check a rule's `initial_columns` (the columns that must lead the table), mirroring the TS
/// validator's `evalInitialColumns`. A required initial column that is absent → `TSV_COLUMN_MISSING`.
///
/// Note: column *ordering* (`TSV_COLUMN_ORDER_INCORRECT`) is not checked because the parsed
/// columns are stored unordered; only presence of required initial columns is enforced.
fn eval_initial_columns(
    rule: &TabularRuleDef,
    rule_path: &str,
    ctx_value: &EvalContext,
    context: &BidsContext,
    schema: &BidsSchema,
    issues: &mut DatasetIssues,
) {
    if !do_selectors_select(&rule.selectors, ctx_value) {
        return;
    }
    let Some(initial) = &rule.initial_columns else {
        return;
    };
    let col_objs = schema.column_objects();
    for key in initial {
        let name = col_objs
            .get(key)
            .and_then(|c| c.get("name"))
            .and_then(|n| n.as_str())
            .unwrap_or(key);
        let level = rule
            .columns
            .get(key)
            .map(RequirementLevel::level_str)
            .unwrap_or("optional");
        if level == "required" && !context.columns.contains_key(name) {
            issues.add(BidsIssue {
                code: "TSV_COLUMN_MISSING".to_string(),
                sub_code: Some(name.to_string()),
                message: "A required column is missing".to_string(),
                severity: Severity::Error,
                location: context.path.clone(),
                rule: Some(rule_path.to_string()),
                sub_message: Some("Required initial column not found".to_string()),
            });
        }
    }
}

/// Evaluate a single tabular data rule, mirroring the TS validator's `evalColumns`:
///   - a *required* rule column that is missing (and not an initial column) → `TSV_COLUMN_MISSING`
///     (error). Missing recommended/optional columns produce nothing.
///   - an *additional* column not defined in the sidecar → `TSV_ADDITIONAL_COLUMNS_UNDEFINED`
///     (warning), `TSV_ADDITIONAL_COLUMNS_NOT_ALLOWED` (error) when the rule forbids extras, or
///     `TSV_ADDITIONAL_COLUMNS_MUST_DEFINE` (error) when extras are only allowed if defined.
fn eval_tabular_rule(
    rule: &TabularRuleDef,
    rule_path: &str,
    ctx_value: &EvalContext,
    context: &BidsContext,
    schema: &BidsSchema,
    issues: &mut DatasetIssues,
) {
    if !do_selectors_select(&rule.selectors, ctx_value) {
        return;
    }

    // Map each rule column's actual header name -> its rule key.
    let col_objs = schema.column_objects();
    let mut lookup: HashMap<String, &String> = HashMap::new();
    for key in rule.columns.keys() {
        let name = col_objs
            .get(key)
            .and_then(|c| c.get("name"))
            .and_then(|n| n.as_str())
            .unwrap_or(key);
        lookup.insert(name.to_string(), key);
    }

    // Every column named by the rule, plus every column actually present.
    let mut names: HashSet<&str> = lookup.keys().map(|s| s.as_str()).collect();
    for name in context.columns.keys() {
        names.insert(name.as_str());
    }

    let add = |issues: &mut DatasetIssues, code: &str, reason: &str, sev: Severity, name: &str| {
        issues.add(BidsIssue {
            code: code.to_string(),
            sub_code: Some(name.to_string()),
            message: reason.to_string(),
            severity: sev,
            location: context.path.clone(),
            rule: Some(rule_path.to_string()),
            sub_message: None,
        });
    };

    for name in names {
        match lookup.get(name) {
            // Additional column (not named by the rule).
            None => {
                let req = rule.additional_columns.as_deref();
                let defined_in_sidecar = context.sidecar.get(name).is_some();
                if req == Some("not_allowed") {
                    add(
                        issues,
                        "TSV_ADDITIONAL_COLUMNS_NOT_ALLOWED",
                        "A TSV file has extra columns which are not allowed for its file type",
                        Severity::Error,
                        name,
                    );
                } else if !defined_in_sidecar {
                    if req == Some("allowed_if_defined") {
                        add(
                            issues,
                            "TSV_ADDITIONAL_COLUMNS_MUST_DEFINE",
                            "Additional TSV columns must be defined in the associated JSON sidecar for this file type",
                            Severity::Error,
                            name,
                        );
                    } else {
                        add(
                            issues,
                            "TSV_ADDITIONAL_COLUMNS_UNDEFINED",
                            "A TSV file has extra columns which are not defined in its associated JSON sidecar",
                            Severity::Warning,
                            name,
                        );
                    }
                }
            }
            // Column named by the rule.
            Some(key) => {
                if !context.columns.contains_key(name) {
                    let level = rule
                        .columns
                        .get(*key)
                        .map(RequirementLevel::level_str)
                        .unwrap_or("optional");
                    let is_initial = rule
                        .initial_columns
                        .as_ref()
                        .is_some_and(|ic| ic.contains(*key));
                    if level == "required" && !is_initial {
                        add(
                            issues,
                            "TSV_COLUMN_MISSING",
                            "A required column is missing",
                            Severity::Error,
                            name,
                        );
                    }
                }
            }
        }
    }
}
