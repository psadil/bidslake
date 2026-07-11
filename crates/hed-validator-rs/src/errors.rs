//! The [`HedError`] issue type and the [`codes`] module of error-code string constants.
//! Codes are plain strings so they compare directly against the JSON conformance fixtures.

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Error, Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[error("[{issue_type}] {issue_code}: {message}")]
pub struct HedError {
    pub issue_type: String,    // e.g. "ERROR" or "WARNING"
    pub issue_code: String,    // e.g. "TAG_INVALID" or "UNITS_INVALID"
    pub issue_subcode: String, // e.g. "invalid-character-name-value-class"
    pub message: String,
    pub location: Option<String>, // the part of the string where it happened
}

impl HedError {
    pub fn new(
        issue_type: &str,
        issue_code: &str,
        issue_subcode: &str,
        message: &str,
        location: Option<String>,
    ) -> Self {
        Self {
            issue_type: issue_type.to_string(),
            issue_code: issue_code.to_string(),
            issue_subcode: issue_subcode.to_string(),
            message: message.to_string(),
            location,
        }
    }

    pub fn error(code: &str, message: &str, location: Option<String>) -> Self {
        Self::new("ERROR", code, "", message, location)
    }

    pub fn warning(code: &str, message: &str, location: Option<String>) -> Self {
        Self::new("WARNING", code, "", message, location)
    }
}

/// Known HED validation error/warning codes, as plain string constants (matching the
/// `issue_code` values used by the hed-tests conformance suite directly).
pub mod codes {
    // Structural / parser
    pub const COMMA_MISSING: &str = "COMMA_MISSING";
    pub const PARENTHESES_MISMATCH: &str = "PARENTHESES_MISMATCH";
    pub const TAG_EMPTY: &str = "TAG_EMPTY";

    // Character / value / unit
    pub const CHARACTER_INVALID: &str = "CHARACTER_INVALID";
    pub const VALUE_INVALID: &str = "VALUE_INVALID";
    pub const UNITS_INVALID: &str = "UNITS_INVALID";
    pub const PLACEHOLDER_INVALID: &str = "PLACEHOLDER_INVALID";

    // Tag resolution
    pub const TAG_INVALID: &str = "TAG_INVALID";
    pub const TAG_REQUIRES_CHILD: &str = "TAG_REQUIRES_CHILD";
    pub const ELEMENT_DEPRECATED: &str = "ELEMENT_DEPRECATED"; // WARNING
    pub const TAG_EXTENDED: &str = "TAG_EXTENDED"; // WARNING
    pub const TAG_EXTENSION_INVALID: &str = "TAG_EXTENSION_INVALID";
    pub const TAG_NAMESPACE_PREFIX_INVALID: &str = "TAG_NAMESPACE_PREFIX_INVALID"; // deferred

    // Groups
    pub const TAG_GROUP_ERROR: &str = "TAG_GROUP_ERROR";
    pub const TEMPORAL_TAG_ERROR: &str = "TEMPORAL_TAG_ERROR";
    pub const TAG_NOT_UNIQUE: &str = "TAG_NOT_UNIQUE";
    pub const TAG_EXPRESSION_REPEATED: &str = "TAG_EXPRESSION_REPEATED";

    // Definitions
    pub const DEFINITION_INVALID: &str = "DEFINITION_INVALID";
    pub const DEF_INVALID: &str = "DEF_INVALID";
    pub const DEF_EXPAND_INVALID: &str = "DEF_EXPAND_INVALID";

    // Sidecar
    pub const SIDECAR_INVALID: &str = "SIDECAR_INVALID";
    pub const SIDECAR_KEY_MISSING: &str = "SIDECAR_KEY_MISSING"; // no in-scope cases
    pub const SIDECAR_BRACES_INVALID: &str = "SIDECAR_BRACES_INVALID";

    // Schema loading
    pub const SCHEMA_LOAD_FAILED: &str = "SCHEMA_LOAD_FAILED";
    pub const SCHEMA_HEADER_INVALID: &str = "SCHEMA_HEADER_INVALID";
    pub const SCHEMA_VERSION_INVALID: &str = "SCHEMA_VERSION_INVALID";
    pub const SCHEMA_LIBRARY_INVALID: &str = "SCHEMA_LIBRARY_INVALID";
    pub const SCHEMA_SECTION_MISSING: &str = "SCHEMA_SECTION_MISSING";
    pub const SCHEMA_DUPLICATE_NAMES: &str = "SCHEMA_DUPLICATE_NAMES";
    pub const WIKI_DELIMITERS_INVALID: &str = "WIKI_DELIMITERS_INVALID";
    pub const WIKI_SEPARATOR_INVALID: &str = "WIKI_SEPARATOR_INVALID";
    pub const WIKI_LINE_START_INVALID: &str = "WIKI_LINE_START_INVALID";
    pub const WIKI_LINE_INVALID: &str = "WIKI_LINE_INVALID";

    // Schema compliance
    pub const SCHEMA_ATTRIBUTE_INVALID: &str = "SCHEMA_ATTRIBUTE_INVALID";
    pub const SCHEMA_ATTRIBUTE_VALUE_INVALID: &str = "SCHEMA_ATTRIBUTE_VALUE_INVALID";
    pub const SCHEMA_CHARACTER_INVALID: &str = "SCHEMA_CHARACTER_INVALID";
    pub const SCHEMA_DEPRECATION_ERROR: &str = "SCHEMA_DEPRECATION_ERROR";
    pub const SCHEMA_DUPLICATE_NODE: &str = "SCHEMA_DUPLICATE_NODE";
    pub const SCHEMA_MISSING_EXTRA_VALUE: &str = "SCHEMA_MISSING_EXTRA_VALUE";
}
