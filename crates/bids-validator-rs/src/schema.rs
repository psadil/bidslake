//! BIDS schema deserialization and access.
//!
//! The BIDS specification is defined by a machine-readable JSON schema.
//! This module deserializes that schema into typed Rust structs and provides
//! access to its components: objects (entities, suffixes, datatypes, metadata, etc.),
//! rules (file rules, checks, sidecars, tabular data), and metadata.

use crate::issues::{Issue, Severity};
use crate::rules::checks::CheckNode;
use crate::rules::dataset_metadata::DatasetMetadataRuleDef;
use crate::rules::directories::DirectoryRule;
use crate::rules::errors::ErrorRule;
use crate::rules::files::FilesRules;
use crate::rules::json::JsonNode;
use crate::rules::sidecars::SidecarNode;
use crate::rules::tabular_data::TabularNode;
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;

/// The BIDS schema, parsed from JSON into strongly typed rules and definitions.
///
/// This struct holds all of the definitions and rules needed to validate a BIDS dataset,
/// including file rules, sidecar rules, entities, datatypes, and checks.
/// TODO: that entities/modality portion of BidsSchema is messy. It may
/// be best to figure out how to work from the metaschema directly.
#[derive(Debug, Clone)]
pub struct BidsSchema {
    /// The raw parsed JSON of the schema.
    pub raw: Value,
    /// The version of the BIDS specification this schema represents.
    pub bids_version: String,
    /// The version of the schema language itself.
    pub schema_version: String,
    /// Rules governing directories.
    pub directory_rules: HashMap<String, HashMap<String, DirectoryRule>>,
    /// Rules for validating file names, paths, and extensions.
    pub file_rules: FilesRules,
    /// Pre-parsed expressions and rules for general validation checks.
    pub check_rules: HashMap<String, CheckNode>,
    /// Rules for sidecar metadata files (JSON).
    pub sidecar_rules: HashMap<String, SidecarNode>,
    /// Generic JSON schema rules.
    pub json_rules: HashMap<String, JsonNode>,
    /// Rules for dataset_description.json.
    pub dataset_metadata_rules: HashMap<String, DatasetMetadataRuleDef>,
    /// Rules for tabular data files (TSV).
    pub tabular_data_rules: HashMap<String, TabularNode>,
    /// Definitions for errors/warnings.
    pub error_rules: HashMap<String, ErrorRule>,

    /// Entity definitions from `schema.objects.entities`, keyed by entity name.
    pub entities: HashMap<String, EntityDef>,
    /// Map from entity literal name to schema key, e.g. `"sub" -> "subject"`.
    pub entity_name_to_key: HashMap<String, String>,
    /// Map from entity schema key to literal name, e.g. `"subject" -> "sub"`.
    pub entity_key_to_name: HashMap<String, String>,
    /// Entity ordering from `schema.rules.entities`.
    pub entity_order: Vec<String>,
    /// The datatype values from `schema.objects.datatypes` (e.g. `["anat", "func", …]`).
    pub known_datatypes: Vec<String>,
    /// Modality → datatypes table from `schema.rules.modalities`.
    pub modalities: HashMap<String, ModalityDef>,
}

impl BidsSchema {
    /// Load the bundled schema — the workspace's single source of truth, owned by the
    /// `bids-schema` crate (vendored in-tree via the `third_party/bids-schema` subtree).
    pub fn bundled() -> Result<Self, SchemaError> {
        let raw: Value = serde_json::from_str(bids_schema::SCHEMA_JSON)?;
        Self::from_value(raw)
    }

    /// Resolve a schema from an optional `--schema` spec:
    ///   - the `BIDS_SCHEMA` environment variable overrides everything (URL or file path);
    ///   - `vX.Y.Z`, `stable`, or `latest` → fetched from the BIDS specification site;
    ///   - an `http(s)://` URL → fetched directly;
    ///   - anything else → treated as a local file path;
    ///   - `None` (and no env override) → the bundled schema.
    pub fn resolve(spec: Option<&str>) -> Result<Self, SchemaError> {
        if let Ok(env_spec) = std::env::var("BIDS_SCHEMA") {
            let env_spec = env_spec.trim();
            if !env_spec.is_empty() {
                return Self::from_spec(env_spec);
            }
        }
        match spec {
            None => Self::bundled(),
            Some(s) => Self::from_spec(s),
        }
    }

    fn from_spec(spec: &str) -> Result<Self, SchemaError> {
        static VERSION_RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
            regex::Regex::new(r"^(v\d+\.\d+\.\d+|stable|latest)$").unwrap()
        });
        if VERSION_RE.is_match(spec) {
            let url = format!("https://bids-specification.readthedocs.io/en/{spec}/schema.json");
            Self::from_url(&url)
        } else if spec.starts_with("http://") || spec.starts_with("https://") {
            Self::from_url(spec)
        } else {
            Self::from_file(Path::new(spec))
        }
    }

    /// Fetch and parse a schema from a URL.
    pub fn from_url(url: &str) -> Result<Self, SchemaError> {
        let body = ureq::get(url)
            .call()
            .map_err(|e| SchemaError::Fetch {
                url: url.to_string(),
                source: Box::new(e),
            })?
            .into_string()?;
        Self::from_json_str(&body)
    }

    /// Load a schema from a file path.
    pub fn from_file(path: &Path) -> Result<Self, SchemaError> {
        let content = std::fs::read_to_string(path)?;
        let raw: Value = serde_json::from_str(&content)?;
        Self::from_value(raw)
    }

    /// Load a schema from a JSON string.
    pub fn from_json_str(json: &str) -> Result<Self, SchemaError> {
        let raw: Value = serde_json::from_str(json)?;
        Self::from_value(raw)
    }

    fn from_value(raw: Value) -> Result<Self, SchemaError> {
        let bids_version = raw
            .get("bids_version")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        let schema_version = raw
            .get("schema_version")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();

        // Parse all rule types once, before the per-file loop
        let directory_rules = Self::directory_rules_from_raw(&raw);
        let file_rules = Self::file_rules_from_raw(&raw);
        let check_rules = Self::check_rules_from_raw(&raw);
        let sidecar_rules = Self::sidecar_rules_from_raw(&raw);
        let json_rules = Self::json_rules_from_raw(&raw);
        let dataset_metadata_rules = Self::dataset_metadata_rules_from_raw(&raw);
        let tabular_data_rules = Self::tabular_data_rules_from_raw(&raw);
        let error_rules = Self::error_rules_from_raw(&raw);

        // Derive the object lookups the per-file checks reference repeatedly.
        let entities = Self::get_entities_from_raw(&raw);
        let entity_key_to_name = entities
            .iter()
            .map(|(k, v)| (k.clone(), v.name.clone()))
            .collect();
        // Reuse the shared derivation in bids-schema (single source of truth) rather
        // than re-deriving the abbreviation→key map from the parsed entity list.
        let entity_name_to_key = bids_schema::context::entity_name_to_key(&raw);
        let entity_order = Self::get_entity_order_from_raw(&raw);
        let known_datatypes = Self::get_known_datatypes_from_raw(&raw);
        let modalities = Self::get_modalities_from_raw(&raw);

        Ok(Self {
            raw,
            bids_version,
            schema_version,
            directory_rules,
            file_rules,
            check_rules,
            sidecar_rules,
            json_rules,
            dataset_metadata_rules,
            tabular_data_rules,
            error_rules,
            entities,
            entity_name_to_key,
            entity_key_to_name,
            entity_order,
            known_datatypes,
            modalities,
        })
    }

    /// Access `schema.rules.directories`.
    pub fn directory_rules_from_raw(
        raw: &Value,
    ) -> HashMap<String, HashMap<String, DirectoryRule>> {
        serde_json::from_value(
            raw.get("rules")
                .unwrap_or(&Value::Null)
                .get("directories")
                .unwrap_or(&Value::Null)
                .clone(),
        )
        .unwrap()
    }

    /// Access `schema.rules.files`.
    pub fn file_rules_from_raw(raw: &Value) -> FilesRules {
        serde_json::from_value(
            raw.get("rules")
                .unwrap_or(&Value::Null)
                .get("files")
                .unwrap_or(&Value::Null)
                .clone(),
        )
        .unwrap()
    }

    /// Access `schema.rules.checks`.
    pub fn check_rules_from_raw(raw: &Value) -> HashMap<String, CheckNode> {
        serde_json::from_value(
            raw.get("rules")
                .unwrap_or(&Value::Null)
                .get("checks")
                .unwrap_or(&Value::Null)
                .clone(),
        )
        .unwrap()
    }

    /// Access `schema.rules.sidecars`.
    pub fn sidecar_rules_from_raw(raw: &Value) -> HashMap<String, SidecarNode> {
        serde_json::from_value(
            raw.get("rules")
                .unwrap_or(&Value::Null)
                .get("sidecars")
                .unwrap_or(&Value::Null)
                .clone(),
        )
        .unwrap()
    }

    pub fn json_rules_from_raw(raw: &Value) -> HashMap<String, JsonNode> {
        serde_json::from_value(
            raw.get("rules")
                .unwrap_or(&Value::Null)
                .get("json")
                .unwrap_or(&Value::Null)
                .clone(),
        )
        .unwrap()
    }

    pub fn dataset_metadata_rules_from_raw(raw: &Value) -> HashMap<String, DatasetMetadataRuleDef> {
        serde_json::from_value(
            raw.get("rules")
                .unwrap_or(&Value::Null)
                .get("dataset_metadata")
                .unwrap_or(&Value::Null)
                .clone(),
        )
        .unwrap()
    }

    /// Access `schema.rules.tabular_data`.
    pub fn tabular_data_rules_from_raw(raw: &Value) -> HashMap<String, TabularNode> {
        serde_json::from_value(
            raw.get("rules")
                .unwrap_or(&Value::Null)
                .get("tabular_data")
                .unwrap_or(&Value::Null)
                .clone(),
        )
        .unwrap()
    }

    /// Access `schema.rules.errors`.
    fn error_rules_from_raw(raw: &Value) -> HashMap<String, ErrorRule> {
        serde_json::from_value(
            raw.get("rules")
                .unwrap_or(&Value::Null)
                .get("errors")
                .unwrap_or(&Value::Null)
                .clone(),
        )
        .unwrap()
    }

    // ---- Accessors for top-level sections ----

    /// Access `schema.meta`.
    pub fn meta(&self) -> &Value {
        self.raw.get("meta").unwrap_or(&Value::Null)
    }

    /// Access `schema.objects`.
    pub fn objects(&self) -> &Value {
        self.raw.get("objects").unwrap_or(&Value::Null)
    }

    /// Access `schema.rules`.
    pub fn rules(&self) -> &Value {
        self.raw.get("rules").unwrap_or(&Value::Null)
    }

    // ---- Convenience accessors (return owned types) ----

    /// Parse `schema.objects.entities` into typed entity definitions.
    fn get_entities_from_raw(raw: &Value) -> HashMap<String, EntityDef> {
        raw.get("objects")
            .and_then(|o| o.get("entities"))
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default()
    }

    /// Access `schema.objects.suffixes`.
    pub fn suffix_objects(&self) -> &Value {
        self.objects().get("suffixes").unwrap_or(&Value::Null)
    }

    /// Access `schema.objects.extensions`.
    pub fn extensions(&self) -> HashMap<String, ExtensionDef> {
        self.objects()
            .get("extensions")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default()
    }

    /// Access `schema.objects.datatypes`.
    pub fn datatype_objects(&self) -> &Value {
        self.objects().get("datatypes").unwrap_or(&Value::Null)
    }

    /// Access `schema.objects.metadata`.
    pub fn metadata_objects(&self) -> &Value {
        self.objects().get("metadata").unwrap_or(&Value::Null)
    }

    /// Resolve a schema metadata key (e.g. `"AtlasName"`) to the actual
    /// JSON field name (e.g. `"Name"`).
    ///
    /// The schema stores metadata definitions with a `"name"` property that
    /// may differ from the key. If the key has no explicit name mapping,
    /// the key itself is returned unchanged.
    pub fn metadata_field_name<'a>(&'a self, key: &'a str) -> &'a str {
        self.metadata_objects()
            .get(key)
            .and_then(|def| def.get("name"))
            .and_then(|n| n.as_str())
            .unwrap_or(key)
    }

    /// Access `schema.objects.columns`.
    pub fn column_objects(&self) -> &Value {
        self.objects().get("columns").unwrap_or(&Value::Null)
    }

    /// Parse the entity ordering list from `schema.rules.entities`.
    fn get_entity_order_from_raw(raw: &Value) -> Vec<String> {
        raw.get("rules")
            .and_then(|r| r.get("entities"))
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default()
    }

    /// Access `schema.rules.metaentities`.
    pub fn metaentities(&self) -> Vec<String> {
        self.rules()
            .get("metaentities")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default()
    }

    /// Parse the modality → datatypes table from `schema.rules.modalities`.
    fn get_modalities_from_raw(raw: &Value) -> HashMap<String, ModalityDef> {
        raw.get("rules")
            .and_then(|r| r.get("modalities"))
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default()
    }

    /// Access `schema.rules.common_principles`.
    pub fn common_principles(&self) -> Vec<String> {
        self.rules()
            .get("common_principles")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default()
    }

    /// Access `schema.meta.associations`.
    pub fn associations(&self) -> &Value {
        self.meta().get("associations").unwrap_or(&Value::Null)
    }

    /// Get the list of datatypes for a given modality.
    /// E.g. "mri" -> ["anat", "func", "dwi", ...]
    pub fn datatypes_for_modality(&self, modality: &str) -> Vec<String> {
        self.modalities
            .get(modality)
            .map(|m| m.datatypes.clone())
            .unwrap_or_default()
    }

    /// The known datatype names from `raw`. Delegates to the shared owner in
    /// `bids-schema` (single source of truth); datatype key == value in the schema.
    fn get_known_datatypes_from_raw(raw: &Value) -> Vec<String> {
        bids_schema::datatypes::datatypes(raw)
    }

    /// Get the set of pseudo-file extensions (extensions ending with `/`).
    /// Delegates to the shared owner in `bids-schema` (single source of truth)
    /// rather than re-deriving from the parsed `ExtensionDef` list.
    pub fn pseudo_file_extensions(&self) -> Vec<String> {
        bids_schema::pseudo_file_extensions(&self.raw)
    }

    /// Resolve a dot-separated schema path (e.g. "rules.files.raw.anat.nonparametric")
    /// into a reference to the schema value at that path.
    pub fn resolve_path(&self, path: &str) -> &Value {
        let parts: Vec<&str> = path.split('.').collect();
        let mut current = &self.raw;
        for part in &parts {
            current = current.get(*part).unwrap_or(&Value::Null);
            if current.is_null() {
                return &Value::Null;
            }
        }
        current
    }

    /// Retrieve an issue definition from the schema.
    /// This first searches `rules.errors` for a matching key, then searches
    /// `rules.checks` for an embedded `issue` object with the matching code.
    pub fn get_issue(&self, code: &str) -> Option<Issue> {
        // Check rules.errors
        if let Some(errors) = self.rules().get("errors").and_then(|e| e.as_object()) {
            // Check if passed string is the key
            if let Some(err_val) = errors.get(code) {
                return Some(Issue {
                    code: err_val
                        .get("code")
                        .and_then(|c| c.as_str())
                        .unwrap_or(code)
                        .to_string(),
                    message: err_val
                        .get("message")
                        .and_then(|m| m.as_str())
                        .unwrap_or("")
                        .to_string(),
                    level: Some(Severity::from(
                        err_val
                            .get("level")
                            .and_then(|l| l.as_str())
                            .unwrap_or("error"),
                    )),
                });
            }
            // Check if passed string is the code
            for (_key, err_val) in errors {
                if let Some(c) = err_val.get("code").and_then(|c| c.as_str())
                    && c == code
                {
                    return Some(Issue {
                        code: c.to_string(),
                        message: err_val
                            .get("message")
                            .and_then(|m| m.as_str())
                            .unwrap_or("")
                            .to_string(),
                        level: Some(Severity::from(
                            err_val
                                .get("level")
                                .and_then(|l| l.as_str())
                                .unwrap_or("error"),
                        )),
                    });
                }
            }
        }

        // Check rules.checks
        if let Some(checks) = self.rules().get("checks").and_then(|c| c.as_object()) {
            for (_category, category_checks) in checks {
                if let Some(cat_obj) = category_checks.as_object() {
                    for (_check_key, check_val) in cat_obj {
                        if let Some(issue_val) = check_val.get("issue") {
                            let issue_code =
                                issue_val.get("code").and_then(|c| c.as_str()).unwrap_or("");
                            if issue_code == code {
                                return Some(Issue {
                                    code: issue_code.to_string(),
                                    message: issue_val
                                        .get("message")
                                        .and_then(|m| m.as_str())
                                        .unwrap_or("")
                                        .to_string(),
                                    level: Some(Severity::from(
                                        issue_val
                                            .get("level")
                                            .and_then(|l| l.as_str())
                                            .unwrap_or("error"),
                                    )),
                                });
                            }
                        }
                    }
                }
            }
        }

        None
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SchemaError {
    #[error("Failed to parse schema JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Failed to read schema file: {0}")]
    Io(#[from] std::io::Error),
    #[error("Failed to fetch schema from {url}: {source}")]
    Fetch {
        url: String,
        source: Box<ureq::Error>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_load_bundled_schema() {
        let schema = BidsSchema::bundled().unwrap();
        assert!(!schema.bids_version.is_empty());
        assert!(!schema.schema_version.is_empty());
    }

    #[test]
    fn test_entity_mappings() {
        let schema = BidsSchema::bundled().unwrap();
        let key_to_name = &schema.entity_key_to_name;
        assert_eq!(key_to_name.get("subject").map(|s| s.as_str()), Some("sub"));
        assert_eq!(key_to_name.get("session").map(|s| s.as_str()), Some("ses"));

        let name_to_key = &schema.entity_name_to_key;
        assert_eq!(name_to_key.get("sub").map(|s| s.as_str()), Some("subject"));
        assert_eq!(name_to_key.get("ses").map(|s| s.as_str()), Some("session"));
    }

    #[test]
    fn test_known_datatypes() {
        let schema = BidsSchema::bundled().unwrap();
        let datatypes = &schema.known_datatypes;
        assert!(datatypes.contains(&"anat".to_string()));
        assert!(datatypes.contains(&"func".to_string()));
    }

    #[test]
    fn test_entity_order() {
        let schema = BidsSchema::bundled().unwrap();
        let order = &schema.entity_order;
        assert!(!order.is_empty());
        assert_eq!(order[0], "subject");
    }

    #[test]
    fn test_resolve_path() {
        let schema = BidsSchema::bundled().unwrap();
        let val = schema.resolve_path("rules.files.raw.anat.nonparametric");
        assert!(val.get("suffixes").is_some());
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct ModalityDef {
    pub datatypes: Vec<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct EntityDef {
    pub name: String,
    pub entity: Option<String>,
    pub display_name: Option<String>,
    pub description: Option<String>,
    #[serde(rename = "type")]
    pub type_: Option<String>,
    pub format: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct ExtensionDef {
    pub value: String,
    pub display_name: Option<String>,
    pub description: Option<String>,
}
