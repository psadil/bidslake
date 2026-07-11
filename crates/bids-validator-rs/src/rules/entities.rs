use crate::context::BidsContext;
use crate::issues::{BidsIssue, DatasetIssues, Severity};
use crate::schema::BidsSchema;
use regex::Regex;
use std::sync::OnceLock;

fn alphanumeric_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[a-zA-Z0-9+]+$").unwrap())
}

fn index_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[0-9]+$").unwrap())
}

pub fn check_entity_rules(context: &BidsContext, schema: &BidsSchema, issues: &mut DatasetIssues) {
    let entity_keys = &context.entity_keys;

    if entity_keys.is_empty() {
        return;
    }
    let entities_map = &context.raw_entities;
    let file_path = &context.path;

    let schema_entities = &schema.entities;
    let name_to_key = &schema.entity_name_to_key;
    let rules_entities = &schema.entity_order;

    let mut last_schema_index: Option<usize> = None;

    for short_name in entity_keys {
        // Find the schema key for this short name (e.g., "sub" -> "subject")
        let schema_key = match name_to_key.get(short_name) {
            Some(key) => key,
            None => {
                issues.add(BidsIssue {
                    code: "UNKNOWN_ENTITY".to_string(),
                    sub_code: None,
                    message: format!("Unknown entity '{}' found in filename.", short_name),
                    severity: Severity::Error,
                    location: file_path.clone(),
                    rule: None,
                    sub_message: None,
                });
                continue;
            }
        };

        // 1. Check order
        if let Some(expected_index) = rules_entities.iter().position(|e| e == schema_key) {
            if let Some(last_idx) = last_schema_index
                && expected_index < last_idx
            {
                issues.add(BidsIssue {
                    code: "ENTITY_ORDER_INCORRECT".to_string(),
                    sub_code: None,
                    message: format!(
                        "Entity '{}' appears out of order in the filename according to the schema.",
                        short_name
                    ),
                    severity: Severity::Error,
                    location: file_path.clone(),
                    rule: None,
                    sub_message: None,
                });
            }
            last_schema_index = Some(expected_index);
        }

        // 2. Check format
        if let Some(entity_def) = schema_entities.get(schema_key)
            && let Some(value) = entities_map.get(short_name)
        {
            if value == "NOENTITY" {
                continue;
            }

            if let Some(format) = &entity_def.format {
                let is_valid = match format.as_str() {
                    "label" => alphanumeric_regex().is_match(value),
                    "index" => index_regex().is_match(value),
                    _ => true, // Unknown formats are assumed valid
                };

                if !is_valid {
                    issues.add(BidsIssue {
                        code: "INVALID_ENTITY_VALUE".to_string(),
                        sub_code: None,
                        message: format!(
                            "Entity '{}' has invalid value '{}'. Expected format: {}",
                            short_name, value, format
                        ),
                        severity: Severity::Error,
                        location: file_path.clone(),
                        rule: None,
                        sub_message: None,
                    });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{BidsContext, DatasetContext};
    use crate::filetree::{BidsFile, FileTree};
    use crate::schema::BidsSchema;

    async fn build_context(filename: &str, schema: &BidsSchema) -> BidsContext {
        let file = BidsFile {
            name: filename.to_string(),
            path: format!("/{}", filename),
            absolute_path: format!("/dummy/{}", filename).into(),
            size: 100,
        };
        let mut tree = FileTree {
            name: String::new(),
            path: "/".to_string(),
            directories: vec![],
            files: vec![],
        };
        tree.files.push(file.clone());
        let mut issues = DatasetIssues::default();
        let ds_ctx = DatasetContext::new(tree, schema, None, &mut issues).await;
        BidsContext::new(&file, &ds_ctx, schema).await
    }

    #[tokio::test]
    async fn test_valid_entity_order_and_format() {
        let schema = BidsSchema::bundled().unwrap();
        let ctx = build_context("sub-01_ses-pre_task-rest_run-01_bold.nii.gz", &schema).await;
        let mut issues = DatasetIssues::default();

        check_entity_rules(&ctx, &schema, &mut issues);

        assert!(
            issues.all().is_empty(),
            "Expected no issues, found {:?}",
            issues.all()
        );
    }

    #[tokio::test]
    async fn test_invalid_entity_order() {
        let schema = BidsSchema::bundled().unwrap();
        // session before subject
        let ctx = build_context("ses-pre_sub-01_task-rest_run-01_bold.nii.gz", &schema).await;
        let mut issues = DatasetIssues::default();

        check_entity_rules(&ctx, &schema, &mut issues);

        let errors = issues.all();
        assert!(!errors.is_empty());
        assert!(errors.iter().any(|i| i.code == "ENTITY_ORDER_INCORRECT"));
    }

    #[tokio::test]
    async fn test_invalid_index_format() {
        let schema = BidsSchema::bundled().unwrap();
        // run is an index, 'ab' is invalid
        let ctx = build_context("sub-01_run-ab_bold.nii.gz", &schema).await;
        let mut issues = DatasetIssues::default();

        check_entity_rules(&ctx, &schema, &mut issues);

        let errors = issues.all();
        assert!(!errors.is_empty());
        assert!(errors.iter().any(|i| i.code == "INVALID_ENTITY_VALUE"));
    }
}
