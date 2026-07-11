use super::ErrorValidator;
use crate::context::{BidsContext, DatasetContext};

pub struct NiftiHeaderUnreadable;

#[async_trait::async_trait]
impl ErrorValidator for NiftiHeaderUnreadable {
    fn key(&self) -> &'static str {
        "NiftiHeaderUnreadable"
    }

    async fn validate_file(&self, context: &BidsContext, _dataset: &DatasetContext) -> bool {
        // If it's supposed to be a nifti, and nifti_header is Null, it's unreadable.
        // The selector match(extension, '^\\.nii(\\.gz)?$') will limit this check appropriately.
        // We also want to differentiate from TooSmall, but for now we just flag it if null.
        context.nifti_header.is_none()
    }
}

pub struct NiftiTooSmall;

#[async_trait::async_trait]
impl ErrorValidator for NiftiTooSmall {
    fn key(&self) -> &'static str {
        "NiftiTooSmall"
    }

    async fn validate_file(&self, context: &BidsContext, _dataset: &DatasetContext) -> bool {
        // Minimum NIfTI header size is 348 bytes.
        context.size < 348
    }
}
