use crate::context::BidsContext;
use crate::expression::{EvalContext, ValueExt, do_selectors_select, evaluate};
use crate::issues::{BidsIssue, DatasetIssues, Issue, Severity};
use crate::schema::BidsSchema;
use serde::Deserialize;
use std::collections::HashMap;

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct CheckRuleDef {
    pub issue: Option<Issue>,
    pub selectors: Option<Vec<String>>,
    pub checks: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
pub enum CheckNode {
    Rule(CheckRuleDef),
    Category(HashMap<String, CheckNode>),
}

/// Apply all schema checks rules to a file context.
pub fn check_expression_rules(
    context: &BidsContext,
    ctx_value: &EvalContext,
    issues: &mut DatasetIssues,
    schema: &BidsSchema,
) {
    for (category, node) in &schema.check_rules {
        apply_check_node(
            node,
            &format!("rules.checks.{}", category),
            ctx_value,
            context,
            issues,
        );
    }
}

fn apply_check_node(
    node: &CheckNode,
    path: &str,
    ctx_value: &EvalContext,
    context: &BidsContext,
    issues: &mut DatasetIssues,
) {
    match node {
        CheckNode::Rule(rule) => {
            eval_check_rule(rule, path, ctx_value, context, issues);
        }
        CheckNode::Category(map) => {
            for (key, child) in map {
                apply_check_node(
                    child,
                    &format!("{}.{}", path, key),
                    ctx_value,
                    context,
                    issues,
                );
            }
        }
    }
}

/// Evaluate a single check rule: if all selectors match, all checks must pass.
fn eval_check_rule(
    rule: &CheckRuleDef,
    rule_path: &str,
    ctx_value: &EvalContext,
    context: &BidsContext,
    issues: &mut DatasetIssues,
) {
    if !do_selectors_select(&rule.selectors, ctx_value) {
        return;
    }

    if let Some(checks) = &rule.checks {
        for check in checks {
            match evaluate(check, ctx_value) {
                Ok(result) => {
                    // A null check fails, exactly as a false one does: "if an expression
                    // (selector or check) evaluates to `null`, the `null` will be interpreted
                    // equivalent to `false` … a `null` check will fail" (bids-specification,
                    // `src/schema/README.md`, "The special value `null`").
                    if !result.is_truthy() {
                        // Check failed — report the issue
                        let (code, message, level) = if let Some(issue) = &rule.issue {
                            (
                                issue.code.clone(),
                                issue.message.clone(),
                                issue.level.unwrap_or(Severity::Error),
                            )
                        } else {
                            (
                                "CHECK_ERROR".to_string(),
                                "Schema check failed".to_string(),
                                Severity::Error,
                            )
                        };

                        issues.add(BidsIssue {
                            code: code.to_string(),
                            sub_code: None,
                            message: message.to_string(),
                            severity: level,
                            location: context.path.clone(),
                            rule: Some(rule_path.to_string()),
                            sub_message: Some(format!("Failed check: {}", check)),
                        });
                    }
                }
                Err(e) => {
                    // An unevaluable check silently disables its rule. Surface it: a schema
                    // expression this evaluator cannot handle is a bug in the evaluator, not a
                    // property of the dataset. `every_schema_expression_evaluates`
                    // (tests/expression_conformance.rs) asserts we never reach here for the
                    // bundled schema.
                    eprintln!("warning: {rule_path}: could not evaluate check `{check}`: {e}");
                    continue;
                }
            }
        }
    }
}
