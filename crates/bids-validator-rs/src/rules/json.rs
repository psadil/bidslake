use crate::context::BidsContext;
use crate::expression::{EvalContext, do_selectors_select};
use crate::issues::DatasetIssues;
use crate::rules::RequirementLevel;
use crate::schema::BidsSchema;
use serde::Deserialize;
use std::collections::HashMap;

#[derive(Debug, Deserialize, Clone)]
pub struct JsonRuleDef {
    pub selectors: Option<Vec<String>>,
    pub fields: HashMap<String, RequirementLevel>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
pub enum JsonNode {
    Rule(JsonRuleDef),
    Category(HashMap<String, JsonNode>),
}

/// Apply JSON rules: check required/recommended/optional metadata fields for specific JSON files.
pub fn check_json_rules(
    context: &BidsContext,
    ctx_value: &EvalContext,
    schema: &BidsSchema,
    issues: &mut DatasetIssues,
) {
    // json rules are odd; while extension == '.json' is a known selector, the assumption is that
    // that selector is generally not present in these rules.
    if context.extension != ".json" {
        return;
    }

    for (category, node) in &schema.json_rules {
        apply_json_node(
            node,
            &format!("rules.json.{}", category),
            ctx_value,
            context,
            schema,
            issues,
        );
    }
}

fn apply_json_node(
    node: &JsonNode,
    path: &str,
    ctx_value: &EvalContext,
    context: &BidsContext,
    schema: &BidsSchema,
    issues: &mut DatasetIssues,
) {
    match node {
        JsonNode::Rule(rule) => {
            eval_json_rule(rule, path, ctx_value, context, schema, issues);
        }
        JsonNode::Category(map) => {
            for (key, child) in map {
                apply_json_node(
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

fn eval_json_rule(
    rule: &JsonRuleDef,
    path: &str,
    ctx_value: &EvalContext,
    context: &BidsContext,
    schema: &BidsSchema,
    issues: &mut DatasetIssues,
) {
    if !do_selectors_select(rule.selectors.as_deref(), ctx_value) {
        return;
    }

    let exempt = crate::rules::derivative_exempt(ctx_value, &rule.selectors);

    // Process fields
    for (field_key, requirement) in &rule.fields {
        // Schema keys (e.g. "AtlasName") may map to a different actual
        // JSON field name (e.g. "Name") via objects.metadata.<key>.name
        let field_name = schema.metadata_field_name(field_key);
        let field_exists = context.json.get(field_name).is_some();

        requirement.handle_presence_requirement(
            issues,
            field_exists,
            field_name,
            crate::rules::FieldKind::Json,
            &context.path,
            path,
            exempt,
        );
    }
}
