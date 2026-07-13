pub mod bval_bvec;
pub mod dataset;
pub mod empty_file;
pub mod gzip;
pub mod hed;
pub mod json;
pub mod nifti;
pub mod system;
pub mod tsv;

use crate::context::{BidsContext, DatasetContext};
use crate::expression::{EvalContext, do_selectors_select};
use crate::issues::{BidsIssue, DatasetIssues, Severity};
use crate::schema::BidsSchema;
use async_trait::async_trait;
use serde::Deserialize;

/// Represents an error defined in `rules.errors`.
#[derive(Debug, Deserialize, Clone)]
pub struct ErrorRule {
    pub code: String,
    pub message: String,
    pub level: Severity,
    pub selectors: Option<Vec<String>>,
}

/// A trait implemented by instances of errors.
#[async_trait]
pub trait ErrorValidator: Send + Sync {
    /// The key in `rules.errors` (e.g. "EmptyFile")
    fn key(&self) -> &'static str;

    /// File-level validation. Returns `true` if the issue is present.
    async fn validate_file(&self, _context: &BidsContext, _dataset: &DatasetContext) -> bool {
        false
    }

    /// Dataset-level validation. Returns a list of paths with the issue.
    async fn validate_dataset(&self, _dataset: &DatasetContext) -> Vec<String> {
        Vec::new()
    }
}

/// Every error validator, as a compile-time dispatch table. The validators are
/// zero-sized unit structs, so this `static` costs nothing to build — unlike the
/// old per-call `Vec<Box<dyn ErrorValidator>>`, which was rebuilt for every file.
pub static VALIDATORS: &[&dyn ErrorValidator] = &[
    // empty_file
    &empty_file::EmptyFile,
    // bval_bvec
    &bval_bvec::BFile,
    &bval_bvec::BvecRowLength,
    &bval_bvec::MalformedBvec,
    &bval_bvec::MalformedBval,
    // dataset
    &dataset::MissingSession,
    &dataset::NoValidDataFoundForSubject,
    &dataset::SidecarWithoutDatafile,
    // gzip
    &gzip::GzNotGzipped,
    // hed: the HED_* keys are handled by the dedicated `hed::check_hed_file` pass
    // (see that module), not this registry.
    // json
    &json::JsonInvalid,
    &json::InvalidJsonEncoding,
    &json::JsonSchemaValidationError,
    // nifti
    &nifti::NiftiHeaderUnreadable,
    &nifti::NiftiTooSmall,
    // system
    &system::InternalError,
    &system::NotIncluded,
    &system::OrphanedSymlink,
    &system::FileRead,
    &system::InaccessibleRemoteFile,
    &system::BrainvisionLinksBroken,
    // tsv
    &tsv::WrongNewLine,
];

/// Check file-level rules.errors.
pub async fn check_rules_errors_files(
    context: &BidsContext,
    ctx_value: &EvalContext<'_>,
    dataset: &DatasetContext,
    schema: &BidsSchema,
    issues: &mut DatasetIssues,
) {
    for validator in VALIDATORS {
        if let Some(err_def) = schema.error_rules.get(validator.key()) {
            if !do_selectors_select(err_def.selectors.as_deref(), ctx_value) {
                continue;
            }

            if validator.validate_file(context, dataset).await {
                issues.add(BidsIssue {
                    code: err_def.code.clone(),
                    sub_code: None,
                    message: err_def.message.clone(),
                    severity: err_def.level,
                    location: context.path.clone(),
                    rule: None,
                    sub_message: None,
                });
            }
        }
    }
}

/// Check dataset-level rules.errors.
pub async fn check_rules_errors_dataset(
    dataset: &DatasetContext,
    schema: &BidsSchema,
    issues: &mut DatasetIssues,
) {
    for validator in VALIDATORS {
        if let Some(err_def) = schema.error_rules.get(validator.key()) {
            let failing_paths = validator.validate_dataset(dataset).await;
            for path in failing_paths {
                issues.add(BidsIssue {
                    code: err_def.code.clone(),
                    sub_code: None,
                    message: err_def.message.clone(),
                    severity: err_def.level,
                    location: path,
                    rule: None,
                    sub_message: None,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn test_all_schema_errors_implemented() {
        let schema = BidsSchema::bundled().unwrap();
        let mut implemented: HashSet<&str> = VALIDATORS.iter().map(|e| e.key()).collect();
        // HED keys are implemented by the dedicated `hed::check_hed_file` pass rather than the
        // generic `ErrorValidator` registry.
        implemented.extend(hed::HED_ERROR_KEYS.iter().copied());
        let schema_errors = schema.rules().get("errors").unwrap().as_object().unwrap();

        let mut missing = Vec::new();
        for (key, _) in schema_errors {
            if !implemented.contains(key.as_str()) {
                missing.push(key.clone());
            }
        }

        assert!(
            missing.is_empty(),
            "Missing implementations for schema errors: {:?}",
            missing
        );
    }
}
