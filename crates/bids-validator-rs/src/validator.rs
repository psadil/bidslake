use std::collections::HashSet;
use std::path::Path;

use futures::stream::{self, StreamExt};

use crate::config::ValidatorConfig;
use crate::context::{BidsContext, DatasetContext};
use crate::expression::EvalContext;
use crate::filetree::read_file_tree;
use crate::issues::DatasetIssues;
use crate::rules::checks::check_expression_rules;
use crate::rules::dataset_metadata::check_dataset_metadata_rules;
use crate::rules::directories::check_directory_rules;
use crate::rules::entities::check_entity_rules;
use crate::rules::errors::hed::check_hed_file;
use crate::rules::errors::{check_rules_errors_dataset, check_rules_errors_files};
use crate::rules::files::check_file_rules;
use crate::rules::json::check_json_rules;
use crate::rules::sidecars::check_sidecar_overrides;
use crate::rules::sidecars::check_sidecar_rules;
use crate::rules::tabular_data::check_tabular_rules;
use crate::schema::BidsSchema;

/// An error returned by [`validate`]. A typed error (with a `source` chain) rather
/// than a `String`, so callers can match on it and `?`-compose it.
#[derive(Debug, thiserror::Error)]
pub enum ValidatorError {
    /// The dataset directory tree could not be read.
    #[error("reading dataset tree at {path}")]
    ReadTree {
        /// The dataset root that could not be read.
        path: std::path::PathBuf,
        /// The underlying IO error.
        source: std::io::Error,
    },
}

/// Validates a BIDS dataset against a schema.
///
/// # Arguments
///
/// * `dataset_path` - Path to the root of the BIDS dataset.
/// * `schema` - The BIDS schema to validate against.
/// * `config` - Optional configuration to apply (e.g., ignored issue codes).
///
/// # Returns
///
/// Returns a `DatasetIssues` object containing all discovered errors and warnings,
/// or a [`ValidatorError`] if the dataset tree could not be read.
pub async fn validate(
    dataset_path: &Path,
    schema: &BidsSchema,
    config: Option<&ValidatorConfig>,
) -> Result<DatasetIssues, ValidatorError> {
    // Collect ignored codes from config
    let mut ignored_codes = HashSet::new();
    if let Some(cfg) = config {
        for rule in &cfg.ignore {
            ignored_codes.insert(rule.code.clone());
        }
    }

    // Read the file tree
    let pseudo_exts = schema.pseudo_file_extensions();
    let tree =
        read_file_tree(dataset_path, &pseudo_exts).map_err(|source| ValidatorError::ReadTree {
            path: dataset_path.to_path_buf(),
            source,
        })?;

    // Prepare main issues collector
    let mut issues = DatasetIssues {
        issues: Vec::new(),
        ignored_codes,
    };

    // Build the dataset context
    let hed_schema_dir = config.and_then(|c| c.hed_schema_dir.as_deref());
    let dataset_ctx = DatasetContext::new(tree, schema, hed_schema_dir, &mut issues).await;

    // Validate directory-level rules
    let mut opaque_dirs = Vec::new();
    check_directory_rules(&dataset_ctx, schema, &mut issues, &mut opaque_dirs);
    check_rules_errors_dataset(&dataset_ctx, schema, &mut issues).await;

    // Clone the base issues (including ignored codes) for use in the concurrent stream
    let base_issues = issues.clone();

    // The `dataset` / `schema` / `subject` expression bindings are the same for every file,
    // so build them once here; each file's `EvalContext` borrows them.
    let dataset_value = dataset_ctx.dataset_context_value();
    let schema_value = dataset_ctx.schema_context_value(schema);
    let subject_value = dataset_ctx.subject_context_value();

    // Filter files to check by omitting those inside opaque directories
    let files_to_check = dataset_ctx.tree.walk_files().filter(|file| {
        !opaque_dirs
            .iter()
            .any(|d| file.path.starts_with(&format!("{}/", d)))
    });

    // Validate each file concurrently
    let file_issues: Vec<DatasetIssues> = stream::iter(files_to_check)
        .map(async |file| {
            let mut local_issues = DatasetIssues {
                issues: Vec::new(),
                ignored_codes: base_issues.ignored_codes.clone(),
            };
            let mut context = BidsContext::new(file, &dataset_ctx, schema).await;

            // Bind this file's fields and pair them with the shared dataset-scope bindings
            // into the `EvalContext` that rule expressions are evaluated against.
            let file_value = context.to_file_value();
            let ctx = EvalContext::new(&file_value, &dataset_value, &schema_value, &subject_value);

            check_dataset_metadata_rules(&context, &ctx, schema, &mut local_issues);

            check_file_rules(&mut context, &ctx, &dataset_ctx, schema, &mut local_issues);
            check_expression_rules(&context, &ctx, &mut local_issues, schema);
            check_sidecar_rules(&context, &ctx, schema, &mut local_issues);
            check_sidecar_overrides(&context, &mut local_issues);
            check_entity_rules(&context, schema, &mut local_issues);
            check_json_rules(&context, &ctx, schema, &mut local_issues);
            check_tabular_rules(&context, &ctx, schema, &mut local_issues);
            check_rules_errors_files(&context, &ctx, &dataset_ctx, schema, &mut local_issues).await;
            check_hed_file(&context, &dataset_ctx, schema, &mut local_issues).await;

            local_issues
        })
        .buffer_unordered(100)
        .collect()
        .await;

    // Merge individual file issues into the main collection
    for fi in file_issues {
        issues.merge(fi);
    }

    // Reported locations for directory pseudo-files (e.g. `*.ds`, `*.ome.zarr`) carry a
    // trailing slash, matching the TS validator. Internal paths stay slash-free so path
    // matching (scans, associations) is unaffected.
    let pseudo_dir_paths: HashSet<String> = dataset_ctx
        .tree
        .walk_files()
        .filter(|f| f.absolute_path.is_dir())
        .map(|f| f.path.clone())
        .collect();
    if !pseudo_dir_paths.is_empty() {
        for issue in &mut issues.issues {
            if pseudo_dir_paths.contains(&issue.location) {
                issue.location.push('/');
            }
        }
    }

    Ok(issues)
}
