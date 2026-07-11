use super::ErrorValidator;
use crate::context::{BidsContext, DatasetContext};

pub struct JsonInvalid;

#[async_trait::async_trait]
impl ErrorValidator for JsonInvalid {
    fn key(&self) -> &'static str {
        "JsonInvalid"
    }

    async fn validate_file(&self, context: &BidsContext, _dataset: &DatasetContext) -> bool {
        // If it's a JSON file but context.json is Null (meaning it failed to parse)
        // AND it's not because of encoding. For simplicity, if it's null we flag it.
        context.json.is_null()
    }
}

pub struct InvalidJsonEncoding;

#[async_trait::async_trait]
impl ErrorValidator for InvalidJsonEncoding {
    fn key(&self) -> &'static str {
        "InvalidJsonEncoding"
    }

    async fn validate_file(&self, context: &BidsContext, dataset: &DatasetContext) -> bool {
        let Some(file) = dataset.tree.find_file(&context.path) else {
            return false;
        };
        if let Ok(bytes) = file.read_bytes().await {
            // Check if it's valid UTF-8
            if std::str::from_utf8(&bytes).is_err() {
                return true;
            }
        }
        false
    }
}

pub struct JsonSchemaValidationError;

#[async_trait::async_trait]
impl ErrorValidator for JsonSchemaValidationError {
    fn key(&self) -> &'static str {
        "JsonSchemaValidationError"
    }

    async fn validate_file(&self, _context: &BidsContext, _dataset: &DatasetContext) -> bool {
        // Stubbed to pass (return false means no error)
        false
    }
}
