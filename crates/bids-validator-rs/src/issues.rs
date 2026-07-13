//! Issue tracking for BIDS validation.
//!
//! Provides types for representing validation issues (errors, warnings, etc.)
//! and a collection to accumulate them during validation.

use std::fmt;

use serde::Deserialize;

/// Severity level of a validation issue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
    Warning,
    Ignore,
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Severity::Error => write!(f, "ERROR"),
            Severity::Warning => write!(f, "WARNING"),
            Severity::Ignore => write!(f, "IGNORE"),
        }
    }
}

/// Infallible parse of a BIDS level string; unknown levels default to `Warning`.
/// Uses the standard conversion trait so callers can write `s.into()` and the type
/// composes with generic `From` bounds.
impl From<&str> for Severity {
    fn from(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "error" => Severity::Error,
            "warning" => Severity::Warning,
            "ignore" => Severity::Ignore,
            _ => Severity::Warning,
        }
    }
}

/// A definition of an issue from the schema.
#[derive(Debug, Clone, Deserialize)]
pub struct Issue {
    pub code: String,
    pub message: String,
    pub level: Option<Severity>,
}

/// A single validation issue.
///
/// Field names mirror the reference TS validator's JSON output: `code`, `sub_code` (`subCode`),
/// `severity`, `location`, `sub_message` (`issueMessage`), `rule`. The generic per-code
/// `message` is surfaced in the top-level `codeMessages` map rather than per issue.
#[derive(Debug, Clone)]
pub struct BidsIssue {
    /// Machine-readable issue code (e.g. "NOT_INCLUDED", "SIDECAR_KEY_RECOMMENDED").
    pub code: String,
    /// Sub-code: the specific field/column/value the issue is about (TS `subCode`).
    pub sub_code: Option<String>,
    /// Human-readable generic message for this code (surfaced via `codeMessages`).
    pub message: String,
    /// Severity level.
    pub severity: Severity,
    /// File path where the issue was found (relative to dataset root).
    pub location: String,
    /// The schema rule that triggered this issue (dot-separated path).
    pub rule: Option<String>,
    /// Additional context message (TS `issueMessage`).
    pub sub_message: Option<String>,
}

impl BidsIssue {
    /// The rule's last dot-separated segment, e.g. `PETMRISequenceSpecifics`. Used so that
    /// config/test ignores can target a specific rule even when several rules share a generic
    /// code like `SIDECAR_KEY_REQUIRED`.
    pub fn rule_name(&self) -> Option<&str> {
        self.rule.as_deref().and_then(|r| r.rsplit('.').next())
    }
}

/// Lowercase severity string matching the TS validator's JSON (`"error"`/`"warning"`).
fn severity_json(s: Severity) -> &'static str {
    match s {
        Severity::Error => "error",
        Severity::Warning => "warning",
        Severity::Ignore => "ignore",
    }
}

impl fmt::Display for BidsIssue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}: {}", self.severity, self.code, self.message)?;
        if let Some(ref sub) = self.sub_message {
            write!(f, " ({})", sub)?;
        }
        write!(f, "\n\t{}", self.location)?;
        if let Some(ref rule) = self.rule {
            write!(f, " (rule: {})", rule)?;
        }
        Ok(())
    }
}

/// Collection of issues accumulated during validation.
#[derive(Debug, Clone, Default)]
pub struct DatasetIssues {
    pub issues: Vec<BidsIssue>,
    pub ignored_codes: std::collections::HashSet<String>,
}

impl DatasetIssues {
    /// Get the active ignored codes.
    pub fn ignored_codes(&self) -> &std::collections::HashSet<String> {
        &self.ignored_codes
    }

    pub fn merge(&mut self, other: DatasetIssues) {
        self.issues.extend(other.issues);
    }

    /// Whether an issue is suppressed: its `code` or its rule's last segment is in the
    /// ignore set. Matching on the rule name lets configs target a specific rule even when
    /// several rules emit a generic code (e.g. `SIDECAR_KEY_REQUIRED`).
    fn issue_ignored(&self, issue: &BidsIssue) -> bool {
        self.ignored_codes.contains(&issue.code)
            || issue
                .rule_name()
                .is_some_and(|r| self.ignored_codes.contains(r))
    }

    /// Add an issue, unless it is ignored.
    pub fn add(&mut self, issue: BidsIssue) {
        if !self.issue_ignored(&issue) {
            self.issues.push(issue);
        }
    }

    /// Check if a specific issue code is ignored.
    pub fn is_ignored(&self, code: &str) -> bool {
        self.ignored_codes.contains(code)
    }

    /// Add an issue from components.
    #[allow(clippy::too_many_arguments)]
    pub fn add_issue(
        &mut self,
        code: &str,
        message: &str,
        severity: Severity,
        location: &str,
        rule: Option<&str>,
        sub_message: Option<&str>,
    ) {
        self.add(BidsIssue {
            code: code.to_string(),
            sub_code: None,
            message: message.to_string(),
            severity,
            location: location.to_string(),
            rule: rule.map(String::from),
            sub_message: sub_message.map(String::from),
        });
    }

    /// Get all issues.
    pub fn all(&self) -> &[BidsIssue] {
        &self.issues
    }

    /// Get only errors.
    pub fn errors(&self) -> Vec<&BidsIssue> {
        self.issues
            .iter()
            .filter(|i| i.severity == Severity::Error)
            .collect()
    }

    /// Get only warnings.
    pub fn warnings(&self) -> Vec<&BidsIssue> {
        self.issues
            .iter()
            .filter(|i| i.severity == Severity::Warning)
            .collect()
    }

    /// Check if there are any errors.
    pub fn has_errors(&self) -> bool {
        self.issues.iter().any(|i| i.severity == Severity::Error)
    }

    /// Total number of issues.
    pub fn len(&self) -> usize {
        self.issues.len()
    }

    /// Whether the collection is empty.
    pub fn is_empty(&self) -> bool {
        self.issues.is_empty()
    }

    /// Format a summary of issues.
    pub fn format_summary(&self) -> String {
        let errors = self.errors().len();
        let warnings = self.warnings().len();
        format!(
            "Validation complete: {} error(s), {} warning(s), {} total issue(s)",
            errors,
            warnings,
            self.issues.len()
        )
    }

    /// Convert all issues to a JSON structure matching the reference TS validator:
    /// `{ "issues": { "issues": [...], "codeMessages": {...} }, "summary": {...} }`.
    /// Each issue object is `{ code, subCode?, severity, location, issueMessage?, rule? }`.
    pub fn to_json(&self) -> serde_json::Value {
        let mut code_messages = serde_json::Map::new();
        let mut items = Vec::with_capacity(self.issues.len());
        for i in &self.issues {
            code_messages
                .entry(i.code.clone())
                .or_insert_with(|| serde_json::Value::String(i.message.clone()));
            let mut o = serde_json::Map::new();
            o.insert("code".into(), serde_json::Value::String(i.code.clone()));
            if let Some(sc) = &i.sub_code {
                o.insert("subCode".into(), serde_json::Value::String(sc.clone()));
            }
            o.insert(
                "severity".into(),
                serde_json::Value::String(severity_json(i.severity).to_string()),
            );
            o.insert(
                "location".into(),
                serde_json::Value::String(i.location.clone()),
            );
            if let Some(im) = &i.sub_message {
                o.insert("issueMessage".into(), serde_json::Value::String(im.clone()));
            }
            if let Some(r) = &i.rule {
                o.insert("rule".into(), serde_json::Value::String(r.clone()));
            }
            items.push(serde_json::Value::Object(o));
        }

        serde_json::json!({
            "issues": {
                "issues": items,
                "codeMessages": code_messages,
            },
            "summary": {
                "errors": self.errors().len(),
                "warnings": self.warnings().len(),
                "total": self.issues.len(),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_severity_display() {
        assert_eq!(format!("{}", Severity::Error), "ERROR");
        assert_eq!(format!("{}", Severity::Warning), "WARNING");
    }

    #[test]
    fn test_dataset_issues() {
        let mut issues = DatasetIssues::default();
        assert!(issues.is_empty());

        issues.add_issue(
            "TEST_ERROR",
            "Test error message",
            Severity::Error,
            "/sub-01/anat/sub-01_T1w.nii.gz",
            None,
            None,
        );
        issues.add_issue(
            "TEST_WARN",
            "Test warning",
            Severity::Warning,
            "/dataset_description.json",
            None,
            None,
        );

        assert_eq!(issues.len(), 2);
        assert!(issues.has_errors());
        assert_eq!(issues.errors().len(), 1);
        assert_eq!(issues.warnings().len(), 1);
    }
}
