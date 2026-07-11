pub mod checks;
pub mod dataset_metadata;
pub mod directories;
pub mod entities;
pub mod errors;
pub mod files;
pub mod json;
pub mod sidecars;
pub mod tabular_data;

use crate::expression::EvalContext;
use crate::issues::{BidsIssue, DatasetIssues, Severity};
use serde::Deserialize;

/// Whether a metadata field lives in a data file's JSON sidecar or in a JSON file validated
/// directly. Determines the issue code (`SIDECAR_KEY_*` vs `JSON_KEY_*`), matching the TS
/// validator.
#[derive(Debug, Clone, Copy)]
pub enum FieldKind {
    Sidecar,
    Json,
}

/// Whether missing metadata fields should be exempt from reporting: true for a derivative
/// dataset when the rule does not explicitly target derivatives. Mirrors the TS validator's
/// `evalJsonCheck` (BIDS derivatives spec: metadata fields are optional unless specified).
pub fn derivative_exempt(ctx_value: &EvalContext, selectors: &Option<Vec<String>>) -> bool {
    let is_derivative = ctx_value
        .get("dataset")
        .and_then(|d| d.get("dataset_description"))
        .and_then(|dd| dd.get("DatasetType"))
        .and_then(|v| v.as_str())
        == Some("derivative");
    let rule_targets_derivative = selectors.as_ref().is_some_and(|ss| {
        ss.iter()
            .any(|s| s.contains("DatasetType") && s.contains("derivative"))
    });
    is_derivative && !rule_targets_derivative
}

/// A custom issue code/message a schema field can carry in place of the generic
/// `SIDECAR_KEY_*` / `JSON_KEY_*` codes (e.g. `Authors` → `NO_AUTHORS`).
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct CustomIssue {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum RequirementLevel {
    String(String),
    Object {
        level: Option<String>,
        level_addendum: Option<String>,
        description_addendum: Option<String>,
        issue: Option<CustomIssue>,
    },
}

impl RequirementLevel {
    pub fn level_str(&self) -> &str {
        match self {
            RequirementLevel::String(s) => s.as_str(),
            RequirementLevel::Object { level, .. } => level.as_deref().unwrap_or("optional"),
        }
    }

    pub fn addendum(&self) -> Option<&str> {
        match self {
            RequirementLevel::String(_) => None,
            RequirementLevel::Object { level_addendum, .. } => level_addendum.as_deref(),
        }
    }

    /// The field's custom issue code/message, if any.
    pub fn custom_issue(&self) -> Option<&CustomIssue> {
        match self {
            RequirementLevel::Object { issue, .. } => issue.as_ref(),
            RequirementLevel::String(_) => None,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn handle_presence_requirement(
        &self,
        issues: &mut DatasetIssues,
        field_present: bool,
        field_name: &str,
        kind: FieldKind,
        location: &str,
        rule_path: &str,
        derivative_exempt: bool,
    ) {
        if field_present {
            return;
        }

        // In a derivative dataset, sidecar/JSON metadata fields are optional unless a rule
        // explicitly targets derivatives — so missing fields are not reported (mirrors the TS
        // validator and the BIDS derivatives spec).
        if derivative_exempt {
            return;
        }

        // Adhere to the schema: only `required` (error) and `recommended` (warning) missing
        // fields are reported. Notably `deprecated` fields are NOT reported when absent — the
        // TS validator does warn them (its severity mapping falls through to a recommended
        // warning), which we deliberately diverge from. See tests/warning_parity.rs.
        let (severity, level) = match self.level_str() {
            "required" => (Severity::Error, "REQUIRED"),
            "recommended" => (Severity::Warning, "RECOMMENDED"),
            _ => return,
        };
        // A field may carry a custom issue code/message (e.g. `Authors` → `NO_AUTHORS`);
        // otherwise use the generic per-kind/level code (mirrors the TS validator).
        let (code, generic) = match self.custom_issue() {
            Some(ci) => (ci.code.clone(), ci.message.clone()),
            None => {
                let code = match kind {
                    FieldKind::Sidecar => format!("SIDECAR_KEY_{level}"),
                    FieldKind::Json => format!("JSON_KEY_{level}"),
                };
                let generic = match (kind, severity) {
                    (FieldKind::Sidecar, Severity::Error) => {
                        "A data file's JSON sidecar is missing a key listed as required."
                    }
                    (FieldKind::Sidecar, _) => {
                        "A data file's JSON sidecar is missing a key listed as recommended."
                    }
                    (FieldKind::Json, Severity::Error) => {
                        "A JSON file is missing a key listed as required."
                    }
                    (FieldKind::Json, _) => "A JSON file is missing a key listed as recommended.",
                };
                (code, generic.to_string())
            }
        };

        let mut detail = format!("Field '{}' is {}", field_name, self.level_str());
        if let Some(addendum) = self.addendum() {
            detail.push_str(" (");
            detail.push_str(addendum);
            detail.push(')');
        }

        issues.add(BidsIssue {
            code,
            sub_code: Some(field_name.to_string()),
            message: generic,
            severity,
            location: location.to_string(),
            rule: Some(rule_path.to_string()),
            sub_message: Some(detail),
        });
    }
}
