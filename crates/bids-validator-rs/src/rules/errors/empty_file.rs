use super::ErrorValidator;
use crate::context::{BidsContext, DatasetContext};

pub struct EmptyFile;

#[async_trait::async_trait]
impl ErrorValidator for EmptyFile {
    fn key(&self) -> &'static str {
        "EmptyFile"
    }

    async fn validate_file(&self, context: &BidsContext, _dataset: &DatasetContext) -> bool {
        context.size == 0
    }
}
