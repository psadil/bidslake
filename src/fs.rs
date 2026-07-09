//! Filesystem abstraction for ingestion.
//!
//! [`BidsFileSystem`] lets the parser walk and read a dataset without caring
//! whether it lives on local disk ([`LocalFileSystem`]) or in S3
//! ([`crate::s3::S3Client`]). All paths returned by `walk` are relative to the
//! dataset root.

use anyhow::Result;
use futures::future::BoxFuture;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// Trait for abstracting file system access (Local vs S3)
pub trait BidsFileSystem: Send + Sync {
    /// List all files in the dataset (recursively)
    /// Returns a list of paths relative to the dataset root
    fn walk(&self) -> BoxFuture<'_, Result<Vec<PathBuf>>>;

    /// Read file content as string
    fn read_to_string(&self, path: &Path) -> BoxFuture<'_, Result<String>>;

    /// Get the root path/URI of the dataset
    fn root(&self) -> String;
}

pub struct LocalFileSystem {
    root: PathBuf,
}

impl LocalFileSystem {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

impl BidsFileSystem for LocalFileSystem {
    fn walk(&self) -> BoxFuture<'_, Result<Vec<PathBuf>>> {
        let root = self.root.clone();
        Box::pin(async move {
            let mut files = Vec::new();
            // WalkDir is synchronous, but that's okay for local FS
            // We could use tokio::fs::read_dir recursively for true async,
            // but blocking a thread for local FS walk is usually acceptable.
            // For strict async correctness we can wrap in spawn_blocking.
            let walk_res = tokio::task::spawn_blocking(move || {
                let mut paths = Vec::new();
                for entry in WalkDir::new(&root).into_iter().filter_map(|e| e.ok()) {
                    if entry.file_type().is_file()
                        && let Ok(rel_path) = entry.path().strip_prefix(&root)
                    {
                        paths.push(rel_path.to_path_buf());
                    }
                }
                paths
            })
            .await?;

            files.extend(walk_res);
            Ok(files)
        })
    }

    fn read_to_string(&self, path: &Path) -> BoxFuture<'_, Result<String>> {
        let full_path = self.root.join(path);
        Box::pin(async move {
            let content = tokio::fs::read_to_string(full_path).await?;
            Ok(content)
        })
    }

    fn root(&self) -> String {
        // Return as file:// URI for consistency with S3 URIs
        let canonical = self
            .root
            .canonicalize()
            .unwrap_or_else(|_| self.root.clone());
        format!("file://{}", canonical.display())
    }
}
