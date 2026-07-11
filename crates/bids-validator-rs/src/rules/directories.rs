use crate::context::DatasetContext;
use crate::filetree::FileTree;
use crate::issues::{DatasetIssues, Severity};
use crate::schema::BidsSchema;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};

use crate::rules::RequirementLevel;

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum SubDirRule {
    Name(String),
    OneOf {
        #[serde(rename = "oneOf")]
        one_of: Vec<String>,
    },
    AnyOf {
        #[serde(rename = "anyOf")]
        any_of: Vec<String>,
    },
}

#[derive(Debug, Clone, Deserialize)]
pub struct DirectoryRule {
    pub name: Option<String>,
    pub entity: Option<String>,
    pub value: Option<String>,
    pub level: Option<RequirementLevel>,
    #[serde(default)]
    pub opaque: bool,
    pub subdirs: Option<Vec<SubDirRule>>,
}

pub fn check_directory_rules(
    dataset_ctx: &DatasetContext,
    schema: &BidsSchema,
    issues: &mut DatasetIssues,
    opaque_dirs: &mut Vec<String>,
) {
    if let Some(rule_group) = schema.directory_rules.get(&dataset_ctx.dataset_type) {
        validate_directory_tree(
            &dataset_ctx.tree,
            rule_group,
            &["root".to_string()],
            schema,
            issues,
            opaque_dirs,
        );
    }
}

fn validate_directory_tree(
    tree: &FileTree,
    rules: &HashMap<String, DirectoryRule>,
    current_rule_keys: &[String],
    schema: &BidsSchema,
    issues: &mut DatasetIssues,
    opaque_dirs: &mut Vec<String>,
) {
    let mut allowed_subdirs = Vec::new();

    // 1. Determine allowed subdirectories based on current_rule_keys
    for key in current_rule_keys {
        if let Some(rule) = rules.get(key)
            && let Some(subdirs) = &rule.subdirs
        {
            for sub in subdirs {
                match sub {
                    SubDirRule::Name(name) => allowed_subdirs.push(name.clone()),
                    SubDirRule::OneOf { one_of } => allowed_subdirs.extend(one_of.clone()),
                    SubDirRule::AnyOf { any_of } => allowed_subdirs.extend(any_of.clone()),
                }
            }
        }
    }

    let mut matched_required = HashSet::new();

    // 2. Validate children of `tree`
    for child_dir in &tree.directories {
        let mut matched_rule_key = None;
        for allowed_key in &allowed_subdirs {
            if let Some(rule) = rules.get(allowed_key)
                && matches_rule(&child_dir.name, rule, schema)
            {
                matched_rule_key = Some(allowed_key.clone());
                break;
            }
        }

        if let Some(key) = matched_rule_key {
            matched_required.insert(key.clone());
            let rule = rules.get(&key).unwrap();
            if !rule.opaque {
                validate_directory_tree(child_dir, rules, &[key], schema, issues, opaque_dirs);
            } else {
                opaque_dirs.push(child_dir.path.clone());
            }
        } else {
            // No matching rule found
            issues.add_issue(
                "INVALID_DIRECTORY",
                &format!(
                    "Directory '{}' is not allowed in this context.",
                    child_dir.name
                ),
                Severity::Error,
                &child_dir.path,
                None,
                None,
            );
        }
    }

    // 3. Check for missing required directories by evaluating structural rules
    for key in current_rule_keys {
        if let Some(rule) = rules.get(key)
            && let Some(subdirs) = &rule.subdirs
        {
            for sub in subdirs {
                match sub {
                    SubDirRule::Name(name) => {
                        if let Some(r) = rules.get(name)
                            && r.level.as_ref().map(|l| l.level_str()) == Some("required")
                            && !matched_required.contains(name)
                        {
                            issues.add_issue(
                                "MISSING_REQUIRED_DIRECTORY",
                                &format!("Required directory matching rule '{}' is missing.", name),
                                Severity::Error,
                                &tree.path,
                                None,
                                None,
                            );
                        }
                    }
                    SubDirRule::OneOf { one_of } | SubDirRule::AnyOf { any_of: one_of } => {
                        let matched_any = one_of.iter().any(|k| matched_required.contains(k));
                        if !matched_any
                            && let Some(req_name) = one_of.iter().find(|&k| {
                                rules
                                    .get(k)
                                    .and_then(|r| r.level.as_ref())
                                    .map(|l| l.level_str())
                                    == Some("required")
                            })
                        {
                            issues.add_issue(
                                "MISSING_REQUIRED_DIRECTORY",
                                &format!(
                                    "Required directory matching rule '{}' is missing.",
                                    req_name
                                ),
                                Severity::Error,
                                &tree.path,
                                None,
                                None,
                            );
                        }
                    }
                }
            }
        }
    }
}

fn matches_rule(dir_name: &str, rule: &DirectoryRule, schema: &BidsSchema) -> bool {
    if let Some(name) = &rule.name {
        return dir_name == name;
    }
    if let Some(entity_name) = &rule.entity {
        if let Some(entity_def) = schema.entities.get(entity_name) {
            let key = entity_def.entity.as_deref().unwrap_or(&entity_def.name);
            let prefix = format!("{}-", key);
            if dir_name.starts_with(&prefix) {
                let value = &dir_name[prefix.len()..];
                // Check format
                if let Some(format) = &entity_def.format {
                    if format == "index" {
                        static RE_INDEX: std::sync::OnceLock<regex::Regex> =
                            std::sync::OnceLock::new();
                        let re = RE_INDEX.get_or_init(|| regex::Regex::new(r"^[0-9]+$").unwrap());
                        if !re.is_match(value) {
                            return false; // fails format
                        }
                    } else if format == "label" {
                        static RE_LABEL: std::sync::OnceLock<regex::Regex> =
                            std::sync::OnceLock::new();
                        let re =
                            RE_LABEL.get_or_init(|| regex::Regex::new(r"^[a-zA-Z0-9]+$").unwrap());
                        if !re.is_match(value) {
                            return false; // fails format
                        }
                    }
                }
                return true;
            }
        }
        return false;
    }
    if let Some(value_name) = &rule.value {
        let plural = format!("{}s", value_name);
        let obj = schema
            .objects()
            .get(value_name)
            .or_else(|| schema.objects().get(&plural));
        if let Some(obj) = obj
            && let Some(map) = obj.as_object()
        {
            return map.contains_key(dir_name);
        }
        return false;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filetree::FileTree;
    use crate::schema::BidsSchema;

    async fn build_dataset_context() -> DatasetContext {
        let mut root = FileTree {
            name: "".to_string(),
            path: "".to_string(),
            files: vec![],
            directories: vec![],
        };

        let sub_01 = FileTree {
            name: "sub-01".to_string(),
            path: "/sub-01".to_string(),
            files: vec![],
            directories: vec![FileTree {
                name: "anat".to_string(),
                path: "/sub-01/anat".to_string(),
                files: vec![],
                directories: vec![],
            }],
        };
        root.directories.push(sub_01);

        let mut issues = DatasetIssues::default();
        let schema = BidsSchema::bundled().unwrap();
        DatasetContext::new(root, &schema, None, &mut issues).await
    }

    #[tokio::test]
    async fn test_valid_directory_tree() {
        let schema = BidsSchema::bundled().unwrap();
        let ds_ctx = build_dataset_context().await;
        let mut issues = DatasetIssues::default();
        let mut opaque_dirs = Vec::new();

        check_directory_rules(&ds_ctx, &schema, &mut issues, &mut opaque_dirs);

        let errors = issues.all();
        assert!(errors.is_empty(), "Expected no issues, found {:?}", errors);
    }
}
