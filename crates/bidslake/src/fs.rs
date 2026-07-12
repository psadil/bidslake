//! Filesystem abstraction for ingestion.
//!
//! [`BidsFileSystem`] lets the parser walk and read a dataset without caring
//! whether it lives on local disk ([`LocalFileSystem`]) or in S3
//! ([`crate::s3::S3Client`]). All paths returned by `walk` are relative to the
//! dataset root.

use anyhow::Result;
use bids_core::filetree::FileTree;
use futures::future::BoxFuture;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

/// Trait for abstracting file system access (Local vs S3)
pub trait BidsFileSystem: Send + Sync {
    /// List all files in the dataset (recursively), as paths relative to the dataset root.
    /// `pseudo_exts` are the schema's pseudo-file extensions (e.g. `.ds/`, `.ome.zarr/`);
    /// directories matching them are emitted as single files rather than descended into.
    fn walk(&self, pseudo_exts: &[String]) -> BoxFuture<'_, Result<Vec<PathBuf>>>;

    /// Read file content as string
    fn read_to_string(&self, path: &Path) -> BoxFuture<'_, Result<String>>;

    /// Read up to `max_bytes` from the start of a file — enough for a header line
    /// without downloading the whole thing. The default reads the entire file
    /// (fine for local disk); remote backends override it with a ranged fetch so
    /// sniffing a header over the network is a small request, not a full download.
    /// The returned prefix may end mid-line and, for byte-ranged reads, mid-UTF-8;
    /// callers must only rely on complete leading lines.
    fn read_head(&self, path: &Path, _max_bytes: usize) -> BoxFuture<'_, Result<String>> {
        self.read_to_string(path)
    }

    /// Resolve a dataset-relative path to a **local filesystem path** that DuckDB's
    /// `read_csv` can open directly. For [`LocalFileSystem`] this is a no-op join
    /// onto the root; a remote backend must download the object to a temp file.
    /// Used by the tabular ingest, which lets DuckDB parse TSVs natively.
    fn materialize(&self, path: &Path) -> BoxFuture<'_, Result<PathBuf>>;

    /// Get the root path/URI of the dataset
    fn root(&self) -> String;

    /// The in-memory BIDS [`FileTree`] for this backend, if one exists on local
    /// disk (populated once [`walk`](Self::walk) has run). Lets callers reuse the
    /// `bids_core` inheritance helpers, which need the whole tree. Backends without
    /// a local tree (S3) return `None`, and the caller falls back to its own path.
    fn file_tree(&self) -> Option<Arc<FileTree>> {
        None
    }
}

pub struct LocalFileSystem {
    root: PathBuf,
    /// The tree produced by the last [`walk`](BidsFileSystem::walk), cached so
    /// [`file_tree`](BidsFileSystem::file_tree) can hand it to bids-core inheritance.
    tree: OnceLock<Arc<FileTree>>,
}

impl LocalFileSystem {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            tree: OnceLock::new(),
        }
    }
}

impl BidsFileSystem for LocalFileSystem {
    fn walk(&self, pseudo_exts: &[String]) -> BoxFuture<'_, Result<Vec<PathBuf>>> {
        let root = self.root.clone();
        let pseudo: Vec<String> = pseudo_exts.to_vec();
        Box::pin(async move {
            // Delegate to the shared `bids-core` walker: it applies `.bidsignore`
            // (including nested ones), hidden-file, and always-ignore (`.git`,
            // `.datalad`, …) rules during the walk. `pseudo_exts` (from the schema)
            // makes opaque directories like `.ds`/`.ome.zarr` come through as single
            // files rather than being descended into. `read_file_tree` is synchronous,
            // so run it on a blocking thread. The returned paths are root-relative with
            // a leading `/`, which we strip to match the dataset-relative frame the rest
            // of the pipeline expects.
            let tree = tokio::task::spawn_blocking(move || {
                bids_core::filetree::read_file_tree(&root, &pseudo)
            })
            .await??;
            // Flatten to the dataset-relative paths the pipeline expects (strip the
            // leading `/` each tree path carries).
            let paths: Vec<PathBuf> = tree
                .walk_files()
                .map(|f| PathBuf::from(f.path.trim_start_matches('/')))
                .collect();
            // Cache the tree so `file_tree()` can share it with bids-core inheritance.
            let _ = self.tree.set(Arc::new(tree));
            Ok(paths)
        })
    }

    fn read_to_string(&self, path: &Path) -> BoxFuture<'_, Result<String>> {
        let full_path = self.root.join(path);
        Box::pin(async move {
            let content = tokio::fs::read_to_string(full_path).await?;
            Ok(content)
        })
    }

    fn read_head(&self, path: &Path, max_bytes: usize) -> BoxFuture<'_, Result<String>> {
        use tokio::io::AsyncReadExt;
        let full_path = self.root.join(path);
        Box::pin(async move {
            // Read at most `max_bytes` — a header line fits easily — rather than the
            // whole file, matching the S3 ranged read.
            let mut file = tokio::fs::File::open(full_path).await?;
            let mut buf = vec![0u8; max_bytes];
            let n = file.read(&mut buf).await?;
            buf.truncate(n);
            Ok(String::from_utf8_lossy(&buf).into_owned())
        })
    }

    fn materialize(&self, path: &Path) -> BoxFuture<'_, Result<PathBuf>> {
        // Already local: hand back the absolute path for DuckDB to read directly.
        let full_path = self.root.join(path);
        Box::pin(async move { Ok(full_path) })
    }

    fn root(&self) -> String {
        // Return as file:// URI for consistency with S3 URIs
        let canonical = self
            .root
            .canonicalize()
            .unwrap_or_else(|_| self.root.clone());
        format!("file://{}", canonical.display())
    }

    fn file_tree(&self) -> Option<Arc<FileTree>> {
        self.tree.get().cloned()
    }
}
