use super::ErrorValidator;
use crate::context::{BidsContext, DatasetContext};

pub struct InternalError;

#[async_trait::async_trait]
impl ErrorValidator for InternalError {
    fn key(&self) -> &'static str {
        "InternalError"
    }

    async fn validate_file(&self, _context: &BidsContext, _dataset: &DatasetContext) -> bool {
        // Will be manually raised when needed.
        false
    }
}

pub struct NotIncluded;

#[async_trait::async_trait]
impl ErrorValidator for NotIncluded {
    fn key(&self) -> &'static str {
        "NotIncluded"
    }

    async fn validate_file(&self, context: &BidsContext, _dataset: &DatasetContext) -> bool {
        context.filename_rules.is_empty()
    }
}

pub struct OrphanedSymlink;

#[async_trait::async_trait]
impl ErrorValidator for OrphanedSymlink {
    fn key(&self) -> &'static str {
        "OrphanedSymlink"
    }

    async fn validate_file(&self, context: &BidsContext, dataset: &DatasetContext) -> bool {
        let Some(file) = dataset.tree.find_file(&context.path) else {
            return false;
        };
        let path = &file.absolute_path;
        if let Ok(metadata) = tokio::fs::symlink_metadata(path).await
            && metadata.file_type().is_symlink()
        {
            // If symlink_metadata succeeds but metadata fails, the referent is missing
            if tokio::fs::metadata(path).await.is_err() {
                return true;
            }
        }
        false
    }
}

pub struct FileRead;

#[async_trait::async_trait]
impl ErrorValidator for FileRead {
    fn key(&self) -> &'static str {
        "FileRead"
    }

    async fn validate_file(&self, context: &BidsContext, dataset: &DatasetContext) -> bool {
        let Some(file) = dataset.tree.find_file(&context.path) else {
            return false;
        };
        let path = &file.absolute_path;
        // Assuming size > 0 but we can't open it or something?
        // Let's just check if fs::File::open fails for a reason other than NotFound (maybe Permissions).
        if let Err(e) = tokio::fs::File::open(path).await
            && e.kind() == std::io::ErrorKind::PermissionDenied
        {
            return true;
        }
        false
    }
}

pub struct InaccessibleRemoteFile;

#[async_trait::async_trait]
impl ErrorValidator for InaccessibleRemoteFile {
    fn key(&self) -> &'static str {
        "InaccessibleRemoteFile"
    }

    async fn validate_file(&self, _context: &BidsContext, _dataset: &DatasetContext) -> bool {
        // Stub
        false
    }
}

pub struct BrainvisionLinksBroken;

#[async_trait::async_trait]
impl ErrorValidator for BrainvisionLinksBroken {
    fn key(&self) -> &'static str {
        "BrainvisionLinksBroken"
    }

    async fn validate_file(&self, context: &BidsContext, dataset: &DatasetContext) -> bool {
        // BrainVision file triplet (*.eeg, *.vhdr, *.vmrk) must all exist.
        // We can check this when we process the .vhdr file.
        let Some(base_path) = context.path.strip_suffix(".vhdr") else {
            return false;
        };
        let eeg_exists = dataset
            .tree
            .find_file(&format!("{}.eeg", base_path))
            .is_some();
        let vmrk_exists = dataset
            .tree
            .find_file(&format!("{}.vmrk", base_path))
            .is_some();
        !eeg_exists || !vmrk_exists
    }
}
