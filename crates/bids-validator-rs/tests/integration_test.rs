//! Integration tests for the BIDS validator.
//!
//! These tests create temporary BIDS datasets in memory and validate them.

mod common;
mod rules;

use bids_validator_rs::expression::{EvalContext, evaluate_bool};
use bids_validator_rs::schema::BidsSchema;
use common::{create_minimal_dataset, tempdir, validate_dataset};
use serde_json::Value;

#[tokio::test]
async fn test_minimal_valid_dataset() {
    let tmp = tempdir();
    create_minimal_dataset(&tmp);

    let issues = validate_dataset(&tmp).await;

    // There should be no errors (warnings are OK)
    let errors = issues.errors();
    if !errors.is_empty() {
        for e in &errors {
            eprintln!("  ERROR: [{}] {} at {}", e.code, e.message, e.location);
        }
    }
    // We may get NOT_INCLUDED warnings for README/CHANGES/participants.tsv,
    // but no hard errors from a minimal valid dataset
}

#[test]
fn test_expression_evaluation_with_context() {
    let ctx = serde_json::json!({
        "modality": "mri",
        "entities": {
            "part": "phase"
        },
        "sidecar": {
            "Units": "rad",
            "RepetitionTime": 2.0,
        },
        "suffix": "bold",
        "extension": ".nii.gz",
        "datatype": "func",
    });
    let null = Value::Null;
    let ctx = EvalContext::file_only(&ctx, &null);

    // Test various BIDS schema expression patterns
    assert!(evaluate_bool("modality == \"mri\"", &ctx).unwrap());
    assert!(evaluate_bool("entities.part == \"phase\"", &ctx).unwrap());
    assert!(evaluate_bool("\"Units\" in sidecar", &ctx).unwrap());
    assert!(!evaluate_bool("\"MissingKey\" in sidecar", &ctx).unwrap());
    assert!(evaluate_bool("sidecar.Units == \"rad\"", &ctx).unwrap());
    assert!(
        evaluate_bool(
            "intersects([sidecar.Units], [\"rad\", \"arbitrary\"])",
            &ctx
        )
        .unwrap()
    );
    assert!(evaluate_bool("match(extension, \"\\.nii(\\.gz)?$\")", &ctx).unwrap());
    assert!(evaluate_bool("sidecar.RepetitionTime > 0", &ctx).unwrap());
}

#[test]
fn test_schema_loads_and_has_rules() {
    let schema = BidsSchema::bundled().unwrap();

    // Verify schema has expected structure
    assert!(!schema.bids_version.is_empty());
    assert!(!schema.schema_version.is_empty());
    assert!(!schema.entity_order.is_empty());
    assert!(!schema.known_datatypes.is_empty());

    // Verify key sections exist
    assert!(schema.objects().is_object());
}

// ---------------------------------------------------------------------------
// bids-examples: one test per example dataset.
//
// The list of datasets is generated at build time by `build.rs` (which scans
// the `tests/data/bids-examples` submodule) and `include!`d below, so a failure
// names the exact dataset. Datasets carrying pre-existing NON-HED validator gaps
// (PET, eyetracking, microscopy, phenotype recognition) get per-dataset ignore
// codes via `extra_ignores`; HED codes are never ignored on the HED datasets, so
// any HED regression turns those tests red.
// ---------------------------------------------------------------------------

use std::sync::LazyLock;
use tokio::sync::Semaphore;

/// The bundled schema is parsed once and shared across every example test.
static SCHEMA: LazyLock<BidsSchema> = LazyLock::new(|| BidsSchema::bundled().unwrap());

/// Bounds how many datasets validate concurrently. Each validation fans out over many
/// files (`buffer_unordered`), so unbounded parallel tests can exhaust file descriptors.
///
/// We first raise the process's file-descriptor soft limit (best effort; macOS ships a low
/// default of 256 but a high hard limit), then size the gate from the limit we actually got:
/// ~200 fds budgeted per concurrent validation, capped at 16. On a locked-down machine that
/// can't raise past 256 this yields a single permit (safe, serial); on a normal machine it
/// allows real parallelism.
static VALIDATION_GATE: LazyLock<Semaphore> = LazyLock::new(|| {
    let limit = rlimit::increase_nofile_limit(65_536).unwrap_or(256);
    let permits = (limit / 200).clamp(1, 16) as usize;
    Semaphore::new(permits)
});

/// Per-dataset *expected* errors. Two kinds live here:
///   - errors the reference TS validator also reports (from the datasets' stub NIfTI files /
///     known dataset issues), e.g. `BOLD_NOT_4D`, `PET_FRAME_CONSISTENCY_*`,
///     `T1W_FILE_WITH_TOO_MANY_DIMENSIONS`;
///   - `PETMRISequenceSpecifics`: a deliberate stricter-than-TS check. The schema requires
///     `NonlinearGradientCorrection` when PET data are present; TS never fires it because it
///     leaves `dataset.modalities` empty, but we enforce it (see the `to_value` comment).
fn extra_ignores(dataset: &str) -> &'static [&'static str] {
    match dataset {
        // The reference validator reports both of these on atlas-4S too.
        "atlas-4S" => &["BOLD_NOT_4D", "REPETITION_TIME_MISMATCH"],
        "pet001" => &[
            "PETMRISequenceSpecifics",
            "PET_FRAME_CONSISTENCY_FRAME_DURATION",
            "PET_FRAME_CONSISTENCY_FRAME_TIMES_START",
        ],
        "pet002" => &["PETMRISequenceSpecifics"],
        "pet003" => &["PETMRISequenceSpecifics"],
        "pet005" => &[
            "PETMRISequenceSpecifics",
            "T1W_FILE_WITH_TOO_MANY_DIMENSIONS",
        ],
        _ => &[],
    }
}

/// Validate a single example dataset, asserting it produces no (non-ignored) errors.
/// Skips (does not fail) when the submodule dataset directory is not present.
async fn run_example(name: &str) {
    use bids_validator_rs::config::{IgnoreRule, ValidatorConfig};
    use std::path::Path;

    let dataset_dir = Path::new("tests/data/bids-examples").join(name);
    if !dataset_dir.is_dir() {
        eprintln!("Skipping {name}: dataset directory not present (submodule not initialized)");
        return;
    }

    let mut config = ValidatorConfig::from_file("tests/data/bids-examples-config.json")
        .expect("base bids-examples config should load");
    for code in extra_ignores(name) {
        config.ignore.push(IgnoreRule {
            code: (*code).to_string(),
        });
    }

    // Resolve HED library schemas from the vendored hed-schemas checkout when present, so HED
    // datasets validate offline and deterministically (falls back to cache/network otherwise).
    let hed_dir = Path::new("lib/hed-validator-rs/tests/hed-schemas");
    if hed_dir.is_dir() {
        config.hed_schema_dir = Some(hed_dir.to_path_buf());
    }

    let issues = {
        let _permit = VALIDATION_GATE.acquire().await.expect("semaphore is open");
        bids_validator_rs::validator::validate(&dataset_dir, &SCHEMA, Some(&config))
            .await
            .expect("validation should not error out")
    };

    let errors = issues.errors();
    assert!(
        errors.is_empty(),
        "{name} produced {} error(s):\n{}",
        errors.len(),
        errors
            .iter()
            .map(|e| format!("  [{}] {} @ {}", e.code, e.message.trim(), e.location))
            .collect::<Vec<_>>()
            .join("\n"),
    );
}

macro_rules! bids_example_test {
    ($fn_name:ident, $dataset:literal) => {
        // Dataset names carry the original casing (e.g. `atlas-4S`), so allow non-snake-case.
        #[allow(non_snake_case)]
        #[tokio::test]
        async fn $fn_name() {
            run_example($dataset).await;
        }
    };
}

// One `#[tokio::test]` per dataset, generated by build.rs from the bids-examples submodule.
include!(concat!(env!("OUT_DIR"), "/bids_examples_tests.rs"));
