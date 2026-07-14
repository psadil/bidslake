//! BIDS **term maps**: declarative projections that map a standardized-but-non-BIDS file
//! path onto BIDS concepts, following BIDS Extension Proposal 043 ("BIDS Term Mapping").
//!
//! A term map is a list of rules; each rule has a PCRE `Template` matched against a
//! dataset-relative path, whose named capture groups bind BIDS entities, plus literal
//! `Entities`/`Concepts`/`Metadata`. Where an [`overlay`](crate::overlay) extends the BIDS
//! schema *vocabulary*, a term map recognizes files that have no BIDS name at all
//! (FreeSurfer's `sub-01/stats/aseg.stats`) and projects each onto a [`FileFacts`] tuple.
//!
//! This module is pure and I/O-free (`&str -> Option<FileFacts>`) so both `bidslake` and the
//! `bids-validator-rs` validator can consume it. It does **not** read file bodies or decide
//! what to do with a matched file — that is the job of bidslake's ingestion schema and
//! content readers. A term-map document is validated against a hand-written JSON-Schema
//! metaschema ([`TERM_MAPPING_METASCHEMA_JSON`]).
//!
//! PCRE is one of the two `Template` syntaxes BEP-043 floats; we pin it (versioned by the
//! document's `BIDSMapVersion`) and support the subset the `regex` crate provides (named
//! groups, optional groups, character classes — no look-around/back-references), which is
//! sufficient to collapse, e.g., FreeSurfer's `sub-01_ses-1` / `sub-01` / `01` subject-dir
//! forms into one rule.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;
use serde_json::{Map, Value};

/// The hand-written JSON-Schema metaschema (draft 2020-12) for term-map documents.
pub const TERM_MAPPING_METASCHEMA_JSON: &str = include_str!("../data/term-mapping-metaschema.json");

/// Capture-group / entity-name aliases: BEP-043 uses the long forms, BIDS keys are short.
const ENTITY_ALIASES: &[(&str, &str)] = &[("subject", "sub"), ("session", "ses")];

fn alias_entity(name: &str) -> &str {
    ENTITY_ALIASES
        .iter()
        .find(|(from, _)| *from == name)
        .map(|(_, to)| *to)
        .unwrap_or(name)
}

/// An error loading or compiling a term map. Typed (not `anyhow`) as this is a library
/// boundary; still composes into `anyhow` via `?`.
#[derive(Debug, thiserror::Error)]
pub enum TermMapError {
    #[error("reading term map {path}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("parsing term map {path} as JSON")]
    Parse {
        path: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("term map does not conform to the term-mapping metaschema:\n{}", .violations.join("\n"))]
    Invalid { violations: Vec<String> },
    #[error("term-map template `{template}` is not a valid regular expression: {source}")]
    BadTemplate {
        template: String,
        #[source]
        source: regex::Error,
    },
}

// ---------------------------------------------------------------------------
// On-disk format (BEP-043 Term Mapping).
// ---------------------------------------------------------------------------

/// A parsed term-map document.
#[derive(Debug, Clone, Deserialize)]
pub struct TermMapFile {
    #[serde(rename = "BIDSVersion", default)]
    pub bids_version: Option<String>,
    #[serde(rename = "BIDSMapVersion", default)]
    pub bids_map_version: Option<String>,
    #[serde(rename = "Mappings", default)]
    pub mappings: Vec<Mapping>,
}

/// One BEP-043 mapping rule.
#[derive(Debug, Clone, Deserialize)]
pub struct Mapping {
    /// A PCRE matched against a dataset-relative path; named groups bind BIDS entities.
    #[serde(rename = "Template")]
    pub template: String,
    /// Literal BIDS entity -> value pairs (constants not captured by the template).
    #[serde(rename = "Entities", default)]
    pub entities: BTreeMap<String, String>,
    #[serde(rename = "Concepts", default)]
    pub concepts: Concepts,
    #[serde(rename = "Metadata", default)]
    pub metadata: Map<String, Value>,
}

/// BEP-043 `Concepts`: the BIDS `datatype`/`suffix` a mapped file represents.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Concepts {
    #[serde(default)]
    pub datatype: Option<String>,
    #[serde(default)]
    pub suffix: Option<String>,
}

// ---------------------------------------------------------------------------
// Classification output.
// ---------------------------------------------------------------------------

/// The BIDS concepts a term map projects onto a path. `datatype`/`suffix`/`extension` feed
/// the ingestion selectors; `entities` populate materialized concept columns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileFacts {
    pub entities: BTreeMap<String, String>,
    pub datatype: Option<String>,
    pub suffix: Option<String>,
    pub extension: Option<String>,
    pub metadata: Map<String, Value>,
}

impl FileFacts {
    /// Look up an entity value by (aliased) BIDS key.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.entities.get(key).map(String::as_str)
    }
}

// ---------------------------------------------------------------------------
// Compiled term map.
// ---------------------------------------------------------------------------

struct CompiledMapping {
    regex: regex::Regex,
    spec: Mapping,
}

/// A compiled, ready-to-classify term map.
pub struct TermMap {
    mappings: Vec<CompiledMapping>,
    set: regex::RegexSet,
}

impl TermMap {
    /// Compile a parsed document. Each `Template` is anchored and compiled as a regex.
    pub fn from_file(file: TermMapFile) -> Result<Self, TermMapError> {
        let mut mappings = Vec::with_capacity(file.mappings.len());
        let mut patterns = Vec::with_capacity(file.mappings.len());
        for m in file.mappings {
            let anchored = format!("^(?:{})$", m.template);
            let regex =
                regex::Regex::new(&anchored).map_err(|source| TermMapError::BadTemplate {
                    template: m.template.clone(),
                    source,
                })?;
            patterns.push(anchored);
            mappings.push(CompiledMapping { regex, spec: m });
        }
        let set = regex::RegexSet::new(&patterns).map_err(|source| TermMapError::BadTemplate {
            template: "<set>".to_string(),
            source,
        })?;
        Ok(TermMap { mappings, set })
    }

    /// Project a dataset-relative path onto BIDS concepts, or `None` if no rule matches.
    pub fn classify(&self, rel_path: &str) -> Option<FileFacts> {
        let idx = self.set.matches(rel_path).into_iter().next()?;
        let mapping = &self.mappings[idx];
        let caps = mapping.regex.captures(rel_path)?;

        // Named capture groups -> entities (aliased to BIDS short keys).
        let mut entities: BTreeMap<String, String> = BTreeMap::new();
        for name in mapping.regex.capture_names().flatten() {
            if let Some(m) = caps.name(name) {
                entities.insert(alias_entity(name).to_string(), m.as_str().to_string());
            }
        }
        // Literal Entities override/augment.
        for (k, v) in &mapping.spec.entities {
            entities.insert(k.clone(), v.clone());
        }

        Some(FileFacts {
            entities,
            datatype: mapping.spec.concepts.datatype.clone(),
            suffix: mapping.spec.concepts.suffix.clone(),
            extension: filename_extension(rel_path),
            metadata: mapping.spec.metadata.clone(),
        })
    }
}

/// The extension of the final path component, from its first `.` (BIDS filename semantics).
fn filename_extension(path: &str) -> Option<String> {
    let fname = path.rsplit('/').next().unwrap_or(path);
    fname.find('.').map(|i| fname[i..].to_string())
}

// ---------------------------------------------------------------------------
// Validation + registry.
// ---------------------------------------------------------------------------

/// Validate a term-map document against [`TERM_MAPPING_METASCHEMA_JSON`]. Returns the list of
/// violations (empty on success).
pub fn validate_term_map(document: &Value) -> Vec<String> {
    let metaschema: Value = serde_json::from_str(TERM_MAPPING_METASCHEMA_JSON)
        .expect("embedded term-mapping metaschema must parse");
    let validator = jsonschema::validator_for(&metaschema)
        .expect("term-mapping metaschema must compile as a JSON Schema");
    let mut violations: Vec<String> = validator
        .iter_errors(document)
        .map(|e| format!("  at `{}`: {e}", e.instance_path()))
        .collect();
    violations.sort();
    violations.dedup();
    violations
}

/// Term maps bidslake ships, addressable by name.
pub const BUNDLED_TERM_MAP_NAMES: &[&str] = &["freesurfer"];

/// The raw JSON source of a bundled term map, or `None` if `name` is not bundled.
pub fn bundled_term_map_source(name: &str) -> Option<&'static str> {
    Some(match name {
        "freesurfer" => include_str!("../data/term-maps/freesurfer.json"),
        _ => return None,
    })
}

/// The parsed+compiled bundled term map for `name` (build-tested, hence `expect`).
pub fn bundled_term_map(name: &str) -> Option<TermMap> {
    let raw = bundled_term_map_source(name)?;
    let file: TermMapFile = serde_json::from_str(raw).expect("bundled term map must be valid JSON");
    Some(TermMap::from_file(file).expect("bundled term map must compile"))
}

/// Read, validate, parse, and compile a term map from disk.
pub fn load_term_map(path: &Path) -> Result<TermMap, TermMapError> {
    let display = path.display().to_string();
    let content = std::fs::read_to_string(path).map_err(|source| TermMapError::Read {
        path: display.clone(),
        source,
    })?;
    let document: Value = serde_json::from_str(&content).map_err(|source| TermMapError::Parse {
        path: display.clone(),
        source,
    })?;
    let violations = validate_term_map(&document);
    if !violations.is_empty() {
        return Err(TermMapError::Invalid { violations });
    }
    let file: TermMapFile =
        serde_json::from_value(document).map_err(|source| TermMapError::Parse {
            path: display,
            source,
        })?;
    TermMap::from_file(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fs() -> TermMap {
        bundled_term_map("freesurfer").expect("bundled")
    }

    #[test]
    fn bundled_term_map_is_metaschema_valid() {
        let raw = bundled_term_map_source("freesurfer").unwrap();
        let doc: Value = serde_json::from_str(raw).unwrap();
        let violations = validate_term_map(&doc);
        assert!(
            violations.is_empty(),
            "freesurfer term map invalid: {violations:?}"
        );
    }

    #[test]
    fn malformed_term_map_is_rejected() {
        // A mapping missing the required `Template`.
        let doc = serde_json::json!({
            "BIDSVersion": "1.11.1", "BIDSMapVersion": "0.1.0",
            "Mappings": [ { "Concepts": { "datatype": "anat" } } ]
        });
        assert!(!validate_term_map(&doc).is_empty());
    }

    #[test]
    fn pcre_collapses_all_subject_dir_forms() {
        let tm = fs();
        for (path, sub, ses) in [
            ("sub-01_ses-1/stats/aseg.stats", "01", Some("1")),
            ("sub-02/stats/aseg.stats", "02", None),
            ("03/stats/aseg.stats", "03", None),
        ] {
            let f = tm
                .classify(path)
                .unwrap_or_else(|| panic!("no match: {path}"));
            assert_eq!(f.get("sub"), Some(sub));
            assert_eq!(f.get("ses"), ses);
            assert_eq!(f.get("seg"), Some("aseg"));
            assert_eq!(f.suffix.as_deref(), Some("segstats"));
        }
    }

    #[test]
    fn aparc_captures_hemi_and_parc_variants() {
        let f = fs()
            .classify("sub-01_ses-1/stats/lh.aparc.a2009s.stats")
            .expect("match");
        assert_eq!(f.get("hemi"), Some("lh"));
        assert_eq!(f.get("parc"), Some("aparc.a2009s"));
        assert_eq!(f.suffix.as_deref(), Some("parcstats"));
    }

    #[test]
    fn surface_and_volume_project_to_anat() {
        let s = fs().classify("bert/surf/lh.thickness").expect("surf");
        assert_eq!(s.datatype.as_deref(), Some("anat"));
        assert_eq!(s.get("hemi"), Some("lh"));
        let v = fs().classify("bert/mri/aparc+aseg.mgz").expect("mri");
        assert_eq!(v.datatype.as_deref(), Some("anat"));
    }

    #[test]
    fn unrelated_path_is_none() {
        assert!(
            fs().classify("sub-01/func/sub-01_task-rest_bold.nii.gz")
                .is_none()
        );
    }
}
