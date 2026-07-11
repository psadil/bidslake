//! JSON file loading and parsing.

use crate::filetree::BidsFile;
use serde_json::Value;

/// Load and parse a JSON file, returning the parsed value.
pub async fn load_json(file: &BidsFile) -> Result<Value, JsonError> {
    let content = file.read_string().await.map_err(JsonError::Io)?;
    let value: Value = serde_json::from_str(&content).map_err(|e| JsonError::Parse {
        path: file.path.clone(),
        source: e,
    })?;
    Ok(value)
}

#[derive(Debug, thiserror::Error)]
pub enum JsonError {
    #[error("Failed to read file: {0}")]
    Io(#[from] std::io::Error),
    #[error("Failed to parse JSON in {path}: {source}")]
    Parse {
        path: String,
        #[source]
        source: serde_json::Error,
    },
}
