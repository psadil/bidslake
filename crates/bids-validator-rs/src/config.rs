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
    /// Bundled layout adapters (e.g. `freesurfer`) whose BEP-043 term maps recognize
    /// standardized *non-BIDS* files, so they are not flagged as "not part of BIDS".
    #[serde(default)]
    pub adapters: Vec<String>,
}

/// Represents a specific issue code to ignore during validation.
#[derive(Debug, Deserialize, Clone)]
pub struct IgnoreRule {
    /// The issue code (e.g., "NOT_INCLUDED").
    pub code: String,
}

/// An error from [`ValidatorConfig::from_file`]. Typed (with a `source` chain)
/// rather than a `String`, so callers can distinguish read from parse failures.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The config file could not be read.
    #[error("reading config file {path}")]
    Read {
        /// The config path that could not be read.
        path: PathBuf,
        /// The underlying IO error.
        source: std::io::Error,
    },
    /// The config file was read but is not valid JSON.
    #[error("parsing config file {path}")]
    Parse {
        /// The config path that failed to parse.
        path: PathBuf,
        /// The underlying JSON error.
        source: serde_json::Error,
    },
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
    /// Returns a [`ConfigError`] if the file cannot be read or if the JSON is malformed.
    pub fn from_file<P: AsRef<Path>>(path: P) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let content = fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        serde_json::from_str(&content).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })
    }
}
