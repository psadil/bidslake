use crate::context::BidsContext;
use crate::expression::{EvalContext, do_selectors_select};
use crate::issues::DatasetIssues;
use crate::rules::RequirementLevel;
use crate::schema::BidsSchema;

use serde::Deserialize;
use std::collections::HashMap;

#[derive(Debug, Deserialize, Clone)]
pub struct DatasetMetadataRuleDef {
    pub selectors: Option<Vec<String>>,
    pub fields: HashMap<String, RequirementLevel>,
}

/// Apply dataset metadata rules: check required/recommended/optional fields in dataset_description.json.
pub fn check_dataset_metadata_rules(
    context: &BidsContext,
    ctx_value: &EvalContext,
    schema: &BidsSchema,
    issues: &mut DatasetIssues,
) {
    for (rule_name, rule) in &schema.dataset_metadata_rules {
        if !do_selectors_select(&rule.selectors, ctx_value) {
            continue;
        }

        let path = format!("rules.dataset_metadata.{}", rule_name);
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
                &path,
                exempt,
            );
        }
    }
}
