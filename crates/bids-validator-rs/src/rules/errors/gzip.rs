use super::ErrorValidator;
use crate::context::{BidsContext, DatasetContext};

pub struct GzNotGzipped;

#[async_trait::async_trait]
impl ErrorValidator for GzNotGzipped {
    fn key(&self) -> &'static str {
        "GzNotGzipped"
    }

    async fn validate_file(&self, context: &BidsContext, _dataset: &DatasetContext) -> bool {
        // context.gzip is Null if it couldn't be parsed as gzip
        context.gzip.is_null()
    }
}
