use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};

/// Configuration options for the BIDS validator.
#[derive(Debug, Deserialize, Default, Clone)]
pub struct ValidatorConfig {
    /// A list of rules (by code) that the validator should ignore.
    #[serde(default)]
    pub ignore: Vec<IgnoreRule>,
    /// Optional path to a local checkout of `hed-standard/hed-schemas`. When set, HED
    /// schemas are resolved from here first; otherwise they come from the on-disk cache
    /// and a network fetch (mirroring hed-python).
    #[serde(default)]
    pub hed_schema_dir: Option<PathBuf>,
}

/// Represents a specific issue code to ignore during validation.
#[derive(Debug, Deserialize, Clone)]
pub struct IgnoreRule {
    /// The issue code (e.g., "NOT_INCLUDED").
    pub code: String,
}

impl ValidatorConfig {
    /// Loads a `ValidatorConfig` from a JSON file.
    ///
    /// # Arguments
    ///
    /// * `path` - The path to the configuration JSON file.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read or if the JSON is malformed.
    pub fn from_file<P: AsRef<Path>>(path: P) -> Result<Self, String> {
        let content = fs::read_to_string(&path).map_err(|e| {
            format!(
                "Failed to read config file {}: {}",
                path.as_ref().display(),
                e
            )
        })?;
        serde_json::from_str(&content).map_err(|e| {
            format!(
                "Failed to parse config file {}: {}",
                path.as_ref().display(),
                e
            )
        })
    }
}
