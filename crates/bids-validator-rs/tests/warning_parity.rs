//! Warning-parity tests: the Rust validator's warnings and `--json` output should match the
//! reference TypeScript validator's structure and codes.
//!
//! These are **pure-Rust** tests (they do not invoke the TS validator / `deno`). Issues were
//! diffed against the TS validator across all 107 `bids-examples` datasets: 59 match exactly,
//! and every remaining difference is a deliberate divergence or an unimplemented check. The
//! full parity status, the mechanism behind each difference, and the upstream discussion are
//! documented in `docs/warning-parity.md`.

use bids_validator_rs::config::ValidatorConfig;
use bids_validator_rs::issues::DatasetIssues;
use bids_validator_rs::schema::BidsSchema;
use std::path::Path;
use std::sync::LazyLock;

static SCHEMA: LazyLock<BidsSchema> = LazyLock::new(|| BidsSchema::bundled().unwrap());

/// Validate an example dataset with the shared base config.
///
/// A missing dataset directory fails rather than yielding `None`. Every test in this file
/// consumed that `None` with an early `return`, so an uninitialized submodule made all five
/// pass without validating anything — and two of them assert only a *negative*, which an
/// empty run satisfies for free.
async fn validate_example(name: &str) -> DatasetIssues {
    let dir = Path::new("tests/data/bids-examples").join(name);
    assert!(
        dir.is_dir(),
        "{name}: dataset directory not present. \
         Run `git submodule update --init tests/data/bids-examples`."
    );
    let config = ValidatorConfig::from_file("tests/data/bids-examples-config.json").ok();
    bids_validator_rs::validator::validate(&dir, &SCHEMA, config.as_ref())
        .await
        .expect("validation should not error out")
}

/// The `--json` output must use the TS validator's shape: `issues.issues` (array),
/// `issues.codeMessages` (object), and per-issue `code` / lowercase `severity` / `subCode`.
#[tokio::test]
async fn test_json_output_structure_matches_ts() {
    let issues = validate_example("ds001").await;
    let json = issues.to_json();

    let items = json["issues"]["issues"]
        .as_array()
        .expect("issues.issues must be an array");
    assert!(
        json["issues"]["codeMessages"].is_object(),
        "codeMessages map"
    );
    assert!(!items.is_empty(), "ds001 should produce issues");

    for item in items {
        let sev = item["severity"].as_str().unwrap();
        assert!(
            matches!(sev, "error" | "warning" | "ignore"),
            "severity must be lowercase TS style, got {sev:?}"
        );
        assert!(item["code"].is_string(), "each issue has a code");
    }
    // A recommended-field warning must carry a subCode (the field name).
    assert!(
        items
            .iter()
            .any(|i| i["code"] == "JSON_KEY_RECOMMENDED" && i["subCode"].is_string()),
        "expected JSON_KEY_RECOMMENDED warnings with a subCode"
    );
}

/// Recommended metadata fields missing from a sidecar/JSON use the TS codes
/// `SIDECAR_KEY_RECOMMENDED` / `JSON_KEY_RECOMMENDED`, not rule-name codes.
#[tokio::test]
async fn test_recommended_field_codes() {
    let issues = validate_example("asl001").await;
    assert!(
        issues
            .warnings()
            .iter()
            .any(|w| w.code == "SIDECAR_KEY_RECOMMENDED" && w.sub_code.is_some()),
        "expected SIDECAR_KEY_RECOMMENDED warnings with a sub_code"
    );
}

/// An events column not defined in the sidecar → `TSV_ADDITIONAL_COLUMNS_UNDEFINED` (warning),
/// matching TS. (`cash_demean` is present in ds001 events but not its sidecar.)
#[tokio::test]
async fn test_additional_undefined_column_warned() {
    let issues = validate_example("ds001").await;
    assert!(
        issues
            .warnings()
            .iter()
            .any(|w| w.code == "TSV_ADDITIONAL_COLUMNS_UNDEFINED"
                && w.sub_code.as_deref() == Some("cash_demean")),
        "expected TSV_ADDITIONAL_COLUMNS_UNDEFINED for the undefined events column"
    );
}

/// Missing *recommended* TSV columns must NOT be warned (TS only reports missing *required*
/// columns). `strain` is a recommended participants column absent from ds001.
///
/// Paired with a positive control, since a validator that stopped checking TSV columns
/// altogether would satisfy the negative for free.
#[tokio::test]
async fn test_recommended_column_not_warned() {
    let issues = validate_example("ds001").await;

    assert!(
        issues
            .warnings()
            .iter()
            .any(|w| w.code == "TSV_ADDITIONAL_COLUMNS_UNDEFINED"),
        "expected the TSV column checks to report something on ds001"
    );
    assert!(
        !issues
            .warnings()
            .iter()
            .any(|w| w.sub_code.as_deref() == Some("strain")),
        "missing recommended TSV columns must not be warned"
    );
}

/// `DatasetType` is auto-defaulted (like TS) so it is never reported as a missing recommended
/// field.
///
/// Paired with a positive control: ds001 *is* missing four other recommended
/// dataset_description fields, so the absence of `DatasetType` among them says something only
/// if the check that would have reported it ran.
#[tokio::test]
async fn test_dataset_type_not_warned() {
    let issues = validate_example("ds001").await;

    assert!(
        issues.warnings().iter().any(|w| {
            w.code == "JSON_KEY_RECOMMENDED"
                && w.location == "/dataset_description.json"
                && w.sub_code.as_deref() == Some("License")
        }),
        "expected JSON_KEY_RECOMMENDED for ds001's missing License"
    );
    assert!(
        !issues
            .all()
            .iter()
            .any(|i| i.sub_code.as_deref() == Some("DatasetType")),
        "DatasetType must not be reported (it is auto-defaulted)"
    );
}
