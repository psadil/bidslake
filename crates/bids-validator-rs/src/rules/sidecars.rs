use crate::context::BidsContext;
use crate::expression::{EvalContext, do_selectors_select};
use crate::issues::{BidsIssue, DatasetIssues, Severity};
use crate::rules::RequirementLevel;
use crate::schema::BidsSchema;
use serde::Deserialize;
use std::collections::HashMap;

#[derive(Debug, Deserialize, Clone)]
pub struct SidecarRuleDef {
    pub selectors: Option<Vec<String>>,
    pub fields: HashMap<String, RequirementLevel>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
pub enum SidecarNode {
    Rule(SidecarRuleDef),
    Category(HashMap<String, SidecarNode>),
}

/// Emit `SIDECAR_FIELD_OVERRIDE` warnings for inherited keys overridden by a more-specific
/// sidecar with a different value (collected during inheritance in `read_sidecars`). Skipped
/// for JSON files, which do not have sidecars of their own (mirrors the TS validator).
pub fn check_sidecar_overrides(context: &BidsContext, issues: &mut DatasetIssues) {
    if context.extension == ".json" {
        return;
    }
    for ov in &context.sidecar_overrides {
        issues.add(BidsIssue {
            code: "SIDECAR_FIELD_OVERRIDE".to_string(),
            sub_code: Some(ov.key.clone()),
            message: "A sidecar key overrides a value inherited from a less specific sidecar."
                .to_string(),
            severity: Severity::Warning,
            location: ov.location.clone(),
            rule: None,
            sub_message: Some(ov.message.clone()),
        });
    }
}

/// Apply sidecar rules: check required/recommended/optional metadata fields.
pub fn check_sidecar_rules(
    context: &BidsContext,
    ctx_value: &EvalContext,
    schema: &BidsSchema,
    issues: &mut DatasetIssues,
) {
    // Sidecar rules apply to data files, not to JSON sidecars or plain text files (which
    // cannot have sidecars of their own). Mirrors the TS validator's `evalJsonCheck` guard.
    if matches!(
        context.extension.as_str(),
        ".json" | "" | ".md" | ".txt" | ".rst" | ".cff"
    ) {
        return;
    }

    for (category, node) in &schema.sidecar_rules {
        apply_sidecar_node(
            node,
            &format!("rules.sidecars.{}", category),
            ctx_value,
            context,
            schema,
            issues,
        );
    }
}

fn apply_sidecar_node(
    node: &SidecarNode,
    path: &str,
    ctx_value: &EvalContext,
    context: &BidsContext,
    schema: &BidsSchema,
    issues: &mut DatasetIssues,
) {
    match node {
        SidecarNode::Rule(rule) => {
            eval_sidecar_rule(rule, path, ctx_value, context, schema, issues);
        }
        SidecarNode::Category(map) => {
            for (key, child) in map {
                apply_sidecar_node(
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

/// Evaluate a single sidecar rule.
fn eval_sidecar_rule(
    rule: &SidecarRuleDef,
    rule_path: &str,
    ctx_value: &EvalContext,
    context: &BidsContext,
    schema: &BidsSchema,
    issues: &mut DatasetIssues,
) {
    if !do_selectors_select(&rule.selectors, ctx_value) {
        return;
    }

    // Selectors passed — check fields
    let sidecar = ctx_value.get("sidecar");
    let exempt = crate::rules::derivative_exempt(ctx_value, &rule.selectors);

    for (field_key, requirement) in &rule.fields {
        let field_name = schema.metadata_field_name(field_key);
        let field_val = sidecar
            .and_then(|s| s.get(field_name))
            .filter(|v| !v.is_null());
        let field_present = field_val.is_some();

        requirement.handle_presence_requirement(
            issues,
            field_present,
            field_name,
            crate::rules::FieldKind::Sidecar,
            &context.path,
            rule_path,
            exempt,
        );

        // Check type and enum if field is present
        if let Some(val) = field_val
            && let Some(def) = schema.metadata_objects().get(field_key)
        {
            // Check enum constraint
            if let Some(enum_vals) = def.get("enum").and_then(|e| e.as_array())
                && !enum_vals.iter().any(|ev| ev == val)
            {
                issues.add(BidsIssue {
                    code: "JSON_SCHEMA_VALIDATION_ERROR".to_string(),
                    sub_code: Some(field_name.to_string()),
                    message: format!(
                        "Metadata field '{}' value '{}' is not one of the allowed values: {:?}",
                        field_name,
                        val,
                        enum_vals
                            .iter()
                            .filter_map(|v| v.as_str())
                            .collect::<Vec<_>>()
                    ),
                    severity: Severity::Error,
                    location: context.path.clone(),
                    rule: Some(rule_path.to_string()),
                    sub_message: None,
                });
            }
        }
    }
}
