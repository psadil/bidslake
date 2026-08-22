//! Filesystem abstraction for ingestion.
//!
//! [`BidsFileSystem`] lets the parser walk and read a dataset without caring
//! whether it lives on local disk ([`LocalFileSystem`]) or in S3 (`s3::S3Client`, present
//! only with the `s3` feature — hence a plain span rather than a link, which would dangle in
//! the default build). All paths returned by `walk` are relative to the
//! dataset root.

use anyhow::{Context as _, Result};
use futures::future::BoxFuture;
use futures::stream::StreamExt as _;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// How many stats are in flight at once — the metadata dial, shared with the directory walk.
///
/// The number that matters is not local throughput — a local stat is ~2 µs — but network
/// latency. On a parallel filesystem each stat is a round trip to a metadata server, and
/// serially that is minutes per million files; overlapping them is what makes recording
/// size and mtime affordable there at all. It is not monotonic, though, which is why it is
/// tunable: past the point a metadata server saturates, more concurrent stats make it slower.
///
/// Public so the latency benchmark overlaps exactly as many as the real backend does; a bench
/// that hardcodes its own width measures its own constant instead of this one.
pub fn stat_concurrency() -> usize {
    crate::concurrency::metadata()
}

/// A local absolute path as the `root_uri` the catalog stores.
///
/// The inverse of [`local_root`], and the reason both live here rather than at either call
/// site: a stored `root_uri` is the only route from a registry row back to an openable file,
/// so a consumer that spells the conversion itself is one typo away from a query that
/// silently matches nothing. `LocalFileSystem::root` is the original of this format.
pub fn root_uri(path: &Path) -> String {
    format!("file://{}", path.display())
}

/// A `file://` root as a local path, or `None` for a root this build cannot reach — an
/// `s3://` one, or anything whose path is not absolute.
pub fn local_root(root_uri: &str) -> Option<PathBuf> {
    root_uri
        .strip_prefix("file://")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
}

/// What a walk can learn about a file besides its path.
///
/// Deliberately not a checksum. Hashing every file would mean *reading* every file, which
/// is a different order of cost from stat-ing it and is not something an index should do
/// by default. Size and mtime are what a consumer needs to answer "has this changed since
/// I looked" — which is what a content-addressed workflow engine asks, and what `verify`
/// can ask of a catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileStat {
    /// Apparent size, as the backend already knows it: `len()` from a local `stat`, or the
    /// object's `Size` from the S3 listing.
    ///
    /// Local stats follow symlinks, so a symlinked or git-annexed file reports its *target's*
    /// size — and a broken symlink yields no [`FileStat`] at all rather than a zero. A
    /// pseudo-file (a directory the schema treats as one datafile: `.ds/`, `.ome.zarr/`) is
    /// stat-ed like any other walked path, so this is the size of the directory inode and
    /// says nothing about the data inside it.
    pub size_bytes: u64,
    /// Nanoseconds since the Unix epoch. Signed, because a pre-1970 mtime is legal and a
    /// backup restore does produce them; `i64` reaches year 2262, which is enough.
    pub mtime_ns: i64,
}

impl FileStat {
    /// From a `std::fs::Metadata`, or `None` if the platform gives no modification time.
    pub fn from_metadata(md: &std::fs::Metadata) -> Option<Self> {
        let mtime = md.modified().ok()?;
        let mtime_ns = match mtime.duration_since(std::time::UNIX_EPOCH) {
            Ok(d) => i64::try_from(d.as_nanos()).ok()?,
            // Before the epoch: `duration_since` errors and carries the magnitude.
            Err(e) => -i64::try_from(e.duration().as_nanos()).ok()?,
        };
        Some(Self {
            size_bytes: md.len(),
            mtime_ns,
        })
    }
}

/// Trait for abstracting file system access (Local vs S3)
pub trait BidsFileSystem: Send + Sync {
    /// List all files in the dataset (recursively), as paths relative to the dataset root.
    /// `pseudo_exts` are the schema's pseudo-file extensions (e.g. `.ds/`, `.ome.zarr/`);
    /// directories matching them are emitted as single files rather than descended into.
    /// `apply_bidsignore = false` walks every file regardless of `.bidsignore` (see
    /// bidslake's `--no-bidsignore`, for indexing overlay-described derivative outputs).
    fn walk(
        &self,
        pseudo_exts: &[String],
        apply_bidsignore: bool,
    ) -> BoxFuture<'_, Result<Vec<PathBuf>>>;

    /// Size and modification time for each of `paths`, in order.
    ///
    /// `None` for a file that has gone or cannot be stat-ed — a walk and a stat are two
    /// moments, and a dataset can change between them. That is data about the tree, not an
    /// error to abort an ingest over.
    ///
    /// Separate from [`Self::walk`] rather than folded into it, because the two backends
    /// come by this differently: S3 already has size and mtime in the listing it just
    /// paged through and answers from memory, while a POSIX filesystem has to ask. Keeping
    /// them apart is also what lets an ingest skip the stat pass entirely (`--no-stat`)
    /// without the walk knowing.
    fn stat_many<'a>(
        &'a self,
        paths: &'a [PathBuf],
    ) -> BoxFuture<'a, Result<Vec<Option<FileStat>>>>;

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

    /// Resolve a dataset-relative path to a source string DuckDB's `read_csv` can
    /// open directly, ready to use verbatim — the canonical absolute local path for
    /// [`LocalFileSystem`], or an `s3://` URL for the S3 backend (served via httpfs,
    /// not downloaded to a temp file). Each impl returns a final source string so
    /// callers never inspect the scheme. Used by the tabular ingest, which lets
    /// DuckDB parse TSVs natively.
    fn read_csv_source(&self, path: &Path) -> BoxFuture<'_, Result<String>>;

    /// Get the root path/URI of the dataset
    fn root(&self) -> String;
}

/// The [`BidsFileSystem`] for a dataset on this machine — every input that is not an
/// `s3://` URI, and the only backend a build without the `s3` feature has.
///
/// One thing it does that the trait does not promise: the root it *reports* is canonicalized
/// while the root it *reads through* is the path as given. `read_to_string` and `stat_many`
/// join onto the original, [`root`](BidsFileSystem::root) and
/// [`read_csv_source`](BidsFileSystem::read_csv_source) onto the symlink-resolved one. So a
/// dataset reached through a symlinked path is cataloged under its real location — which is
/// what makes a second ingest by either spelling land on the same `root_uri`, and so on the
/// same `file_id` for every file, rather than duplicating the dataset.
pub struct LocalFileSystem {
    root: PathBuf,
    /// A delay charged once per directory during the walk, for benchmarks that model a
    /// network filesystem. `None` in every production path; see
    /// [`with_walk_latency`](Self::with_walk_latency).
    walk_latency: Option<std::time::Duration>,
    /// `root` with symlinks resolved, computed at most once. Every absolute path
    /// this backend hands out is this joined with a dataset-relative path, which is
    /// also exactly what the walk stores in `BidsFile::absolute_path`.
    canonical_root: OnceLock<PathBuf>,
}

impl LocalFileSystem {
    /// A backend rooted at `root` — the directory holding `dataset_description.json`, or, for
    /// a dataset read through a layout adapter, the top of the producer's tree.
    ///
    /// Touches the filesystem not at all: the root is neither canonicalized nor checked to
    /// exist, so a mistyped path is reported by [`walk`](BidsFileSystem::walk) (which names
    /// the root in its error) rather than here.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            walk_latency: None,
            canonical_root: OnceLock::new(),
        }
    }

    /// A backend that sleeps `per_dir` before recording each directory it walks.
    ///
    /// For benchmarks only. A walk's cost on a parallel filesystem is one metadata round trip
    /// per directory, and that is the one part of an ingest no other injection point can reach
    /// — `stat_many` and the body reads happen after the walk has already finished. Charging
    /// the delay inside the walk loop is also what makes the serial-versus-parallel question
    /// measurable: a serial walk costs `dirs × per_dir`, a parallel one `dirs / threads × per_dir`.
    pub fn with_walk_latency(root: impl Into<PathBuf>, per_dir: std::time::Duration) -> Self {
        Self {
            walk_latency: Some(per_dir),
            ..Self::new(root)
        }
    }

    /// The canonical dataset root, resolved once and reused.
    ///
    /// Resolving it per file was the alternative, and on a network filesystem that
    /// is not free: `realpath` walks the path one component at a time, so every
    /// tabular file cost a round trip per directory in its path, serially. The root
    /// is the only part that can need resolving — the rest is a relative path the
    /// walk already produced — so resolving it once is equivalent and O(1).
    ///
    /// Falls back to the path as given when it cannot be resolved, as before.
    fn canonical_root(&self) -> &Path {
        self.canonical_root.get_or_init(|| {
            self.root
                .canonicalize()
                .unwrap_or_else(|_| self.root.clone())
        })
    }
}

impl BidsFileSystem for LocalFileSystem {
    fn walk(
        &self,
        pseudo_exts: &[String],
        apply_bidsignore: bool,
    ) -> BoxFuture<'_, Result<Vec<PathBuf>>> {
        let root = self.root.clone();
        let pseudo: Vec<String> = pseudo_exts.to_vec();
        let latency = self.walk_latency;
        Box::pin(async move {
            // Delegate to the shared `bids-core` walker: it applies `.bidsignore`
            // (including nested ones) unless `apply_bidsignore` is false, plus
            // hidden-file and always-ignore (`.git`, `.datalad`, …) rules during the
            // walk. `pseudo_exts` (from the schema) makes opaque directories like
            // `.ds`/`.ome.zarr` come through as single files rather than being
            // descended into. `read_file_tree` is synchronous, so run it on a blocking
            // thread. The returned paths are root-relative with a leading `/`, which we
            // strip to match the dataset-relative frame the rest of the pipeline expects.
            // `with_context` on the walk, not only at the call site: a missing or
            // unreadable input reports `No such file or directory (os error 2)` with no
            // hint of *which* directory, and that is the first thing a mistyped `--input`
            // produces. The root is the one fact the caller needs and the only one this
            // frame has.
            let root_for_msg = root.clone();
            let dirs = std::sync::atomic::AtomicU64::new(0);
            // `walk_paths`, not `read_file_tree`: an ingest never reads the tree. It used to
            // build one, flatten it straight back into this list, and then hold it for the
            // rest of the run — roughly 180 MB of duplicated path strings at half a million
            // files, kept alive for a consumer that no longer existed.
            //
            // The directory count comes from the walk's own hook rather than from re-walking
            // the finished tree, which was a second full traversal that ran whether or not
            // timing was on.
            let (paths, dirs) = tokio::task::spawn_blocking(move || {
                let paths =
                    bids_core::filetree::walk_paths(&root, &pseudo, apply_bidsignore, &|_| {
                        dirs.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        if let Some(d) = latency {
                            std::thread::sleep(d);
                        }
                    });
                let n = dirs.load(std::sync::atomic::Ordering::Relaxed);
                (paths, n)
            })
            .await?;
            let paths = paths.with_context(|| format!("walking {}", root_for_msg.display()))?;
            crate::timing::count(crate::timing::Counter::Dirs, dirs);
            // Dataset-relative, as the rest of the pipeline expects (strip the leading `/`).
            Ok(paths
                .into_iter()
                .map(|p| PathBuf::from(p.trim_start_matches('/')))
                .collect())
        })
    }

    fn stat_many<'a>(
        &'a self,
        paths: &'a [PathBuf],
    ) -> BoxFuture<'a, Result<Vec<Option<FileStat>>>> {
        Box::pin(async move {
            // Concurrent, not serial, and the reason is latency rather than throughput.
            // A local stat is a couple of microseconds and the ordering would not matter;
            // on a parallel filesystem it is a round trip to a metadata server, and a
            // million serialized round trips is minutes. `buffered` (not `_unordered`)
            // keeps the results aligned with `paths`, so the caller needs no key.
            //
            // `spawn_blocking` because `std::fs::metadata` blocks: without it, sixteen
            // in-flight stats would still take turns on the async worker thread and the
            // concurrency would be a fiction.
            let root = self.root.clone();
            let stats = futures::stream::iter(paths.iter().cloned())
                .map(|rel| {
                    let full = root.join(&rel);
                    async move {
                        tokio::task::spawn_blocking(move || {
                            std::fs::metadata(&full)
                                .ok()
                                .as_ref()
                                .and_then(FileStat::from_metadata)
                        })
                        .await
                        .unwrap_or(None)
                    }
                })
                .buffered(stat_concurrency())
                .collect::<Vec<_>>()
                .await;
            Ok(stats)
        })
    }

    fn read_to_string(&self, path: &Path) -> BoxFuture<'_, Result<String>> {
        let full_path = self.root.join(path);
        Box::pin(async move {
            let content = tokio::fs::read_to_string(&full_path)
                .await
                .with_context(|| format!("reading {}", full_path.display()))?;
            Ok(content)
        })
    }

    fn read_head(&self, path: &Path, max_bytes: usize) -> BoxFuture<'_, Result<String>> {
        use tokio::io::AsyncReadExt;
        let full_path = self.root.join(path);
        Box::pin(async move {
            // Read at most `max_bytes` — a header line fits easily — rather than the
            // whole file, matching the S3 ranged read.
            //
            // Looped, because `read` is permitted to return fewer bytes than asked for
            // whenever it likes and does so routinely on a network filesystem. A single call
            // silently truncated the header line, and the header line is not incidental here:
            // it is the key the batched tabular ingest groups files by *and* the source of the
            // column names, so a short read produced a wrong catalog rather than an error.
            //
            // `read_buf`-style accumulation rather than `read_to_end`, since the point is to
            // stop at `max_bytes`. Reading stops early at EOF (`n == 0`).
            let mut file = tokio::fs::File::open(&full_path)
                .await
                .with_context(|| format!("opening {}", full_path.display()))?;
            let mut buf = Vec::with_capacity(max_bytes.min(64 * 1024));
            let mut chunk = [0u8; 8192];
            while buf.len() < max_bytes {
                let want = chunk.len().min(max_bytes - buf.len());
                let n = file
                    .read(&mut chunk[..want])
                    .await
                    .with_context(|| format!("reading {}", full_path.display()))?;
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                // A complete first line is all any caller relies on, so stop as soon as one
                // is in hand rather than filling the whole budget.
                if buf.contains(&b'\n') {
                    break;
                }
            }
            Ok(String::from_utf8_lossy(&buf).into_owned())
        })
    }

    fn read_csv_source(&self, path: &Path) -> BoxFuture<'_, Result<String>> {
        // Already local: hand back an absolute path for DuckDB to read directly.
        // Anchored on the canonical root so the source is stable regardless of the
        // process's working directory — the same path the walk recorded in
        // `BidsFile::absolute_path`, and the path every other consumer already reads
        // through.
        let full_path = self.canonical_root().join(path);
        Box::pin(async move { Ok(full_path.to_string_lossy().into_owned()) })
    }

    fn root(&self) -> String {
        // Return as file:// URI for consistency with S3 URIs
        format!("file://{}", self.canonical_root().display())
    }
}
