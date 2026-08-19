//! File tree representation and directory walking.
//!
//! Reads a directory on disk into an in-memory tree of files and directories,
//! respecting `.bidsignore` patterns.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// A node representing a directory in the BIDS dataset.
#[derive(Debug, Clone)]
pub struct FileTree {
    /// Name of this directory (e.g. "sub-01").
    pub name: String,
    /// Path relative to the dataset root (e.g. "/sub-01/anat"). Always uses `/`.
    pub path: String,
    /// Files directly contained in this directory.
    pub files: Vec<BidsFile>,
    /// Subdirectories.
    pub directories: Vec<FileTree>,
}

/// A file within the BIDS dataset.
#[derive(Debug, Clone)]
pub struct BidsFile {
    /// Filename (e.g. "sub-01_T1w.nii.gz").
    pub name: String,
    /// Path relative to the dataset root, using `/` separators (e.g. "/sub-01/anat/sub-01_T1w.nii.gz").
    pub path: String,
    /// Absolute path on disk.
    pub absolute_path: PathBuf,
}

/// Directories that should always be ignored during BIDS validation.
const ALWAYS_IGNORE: &[&str] = &[
    "!.git",
    "!.datalad",
    "!.bidsignore",
    "!.gitattributes",
    "!.gitignore",
    "!.bids-validator-config.json",
];

fn get_mut_subtree<'a>(tree: &'a mut FileTree, rel_path: &str) -> Option<&'a mut FileTree> {
    if rel_path == "/" {
        return Some(tree);
    }

    let parts: Vec<&str> = rel_path
        .strip_prefix('/')
        .unwrap_or(rel_path)
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();

    let mut current = tree;
    for part in parts {
        // Searched from the back, which is the whole difference between linear and quadratic
        // here. `read_file_tree` calls this once per walked entry, and `ignore::Walk` is a
        // pre-order DFS: while its files are being yielded, a directory is the most recently
        // pushed child of its parent, and so is every directory on the path from the root down
        // to it. A forward scan therefore hit `position`'s worst case on *every* entry — every
        // file under `sub-2000` compared 2,000 names at the root alone, making tree
        // construction cost `entries × root_fanout`.
        //
        // Equivalent, not merely faster: a directory node is pushed exactly once (the walker
        // yields each directory once), so names are unique among a parent's children and the
        // first match and the last are the same one. The scan is still linear, so an entry
        // arriving out of DFS order — or none at all, for a pruned pseudo-file's contents,
        // which must still return `None` — behaves exactly as before.
        let idx = current.directories.iter().rposition(|d| d.name == part)?;
        current = &mut current.directories[idx];
    }
    Some(current)
}

/// One walker thread's entries, merged into the shared list when the thread finishes.
///
/// `ignore`'s parallel walker builds one closure per thread and drops it when that thread is
/// done, which is the hook this uses: the lock is taken once per thread rather than once per
/// entry.
struct Batch<'a> {
    local: Vec<(PathBuf, std::fs::FileType)>,
    sink: &'a std::sync::Mutex<Vec<(PathBuf, std::fs::FileType)>>,
}

impl Drop for Batch<'_> {
    fn drop(&mut self) {
        if !self.local.is_empty() {
            self.sink.lock().unwrap().append(&mut self.local);
        }
    }
}

/// Order two paths the way a sorted depth-first walk emits them.
///
/// Component-wise, which is what makes a parent sort before its children (its component list
/// is a prefix of theirs) and siblings sort by name. Comparing the paths *as strings* would
/// not: `/a-b` precedes `/a/b` because `-` < `/`, putting a subtree's sibling in the middle of
/// it.
///
/// Written as a comparator over the two iterators rather than by collecting each path's
/// components into a `Vec`, because the collecting version allocates twice per comparison —
/// about 2.8M allocations sorting 80,000 paths, which cost more than the parallel walk saved.
fn depth_first_order(a: &Path, b: &Path) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let mut ac = a.components();
    let mut bc = b.components();
    loop {
        match (ac.next(), bc.next()) {
            (Some(x), Some(y)) => match x.cmp(&y) {
                Ordering::Equal => continue,
                other => return other,
            },
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
        }
    }
}

/// How many threads the walk uses.
///
/// A walk is bounded by `readdir` latency far more than by CPU, so this is not a core count so
/// much as a count of round trips to keep outstanding: on a network filesystem a serial walk
/// costs `directories × RTT`, which is minutes for a large tree. Capped because the returns
/// flatten and a hundred threads on a shared metadata server is antisocial.
fn walk_threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(2, 16)
}

/// Whether any ancestor of `path` below `root` is a pseudo-file directory.
///
/// Tests the ancestors only, never the entry itself, so a pseudo-file directory is still
/// yielded by the walk (it becomes a [`BidsFile`]) while everything beneath it is pruned.
fn has_pseudo_ancestor(path: &Path, root: &Path, pseudo_exts: &[String]) -> bool {
    if pseudo_exts.is_empty() {
        return false;
    }
    let Ok(rel) = path.strip_prefix(root) else {
        return false;
    };
    let mut components: Vec<_> = rel.components().collect();
    components.pop(); // the entry itself
    components
        .iter()
        .any(|c| is_pseudo_file(&c.as_os_str().to_string_lossy(), pseudo_exts))
}

/// Read a directory into a `FileTree`, respecting `.bidsignore` patterns unless
/// `apply_bidsignore` is false.
///
/// The `root` path should point to the top of the BIDS dataset (the directory
/// containing `dataset_description.json`). Passing `apply_bidsignore = false` walks
/// every file regardless of `.bidsignore` — used to index "bidsish" derivative
/// outputs a pipeline's `.bidsignore` hides (see bidslake's `--no-bidsignore`).
pub fn read_file_tree(
    root: &Path,
    pseudo_exts: &[String],
    apply_bidsignore: bool,
) -> Result<FileTree, std::io::Error> {
    read_file_tree_observed(root, pseudo_exts, apply_bidsignore, &|_| {})
}

/// [`read_file_tree`], calling `on_dir` once for each directory the walk descends into.
///
/// The hook exists because a directory is where a walk spends its round trips: on a network
/// filesystem the cost of the traversal is one `readdir` per directory, and nothing else about
/// the walk is observable from outside it. Two callers want that.
///
/// Production counts. bidslake reports `dirs` in its timing breakdown, and counting here is
/// what lets it stop re-walking the finished tree to find the same number — a traversal it was
/// paying on every index, timing on or off.
///
/// Benchmarks inject latency. `bidslake::fs::LocalFileSystem::with_walk_latency` sleeps in this
/// hook, which is the only way to model a network walk deterministically — and the only way to
/// tell a serial walk from a parallel one, since a serial walk costs `dirs × delay` and a
/// parallel one `dirs / threads × delay`.
///
/// `on_dir` is `Sync` so this signature does not have to change if the walk itself becomes
/// parallel.
pub fn read_file_tree_observed(
    root: &Path,
    pseudo_exts: &[String],
    apply_bidsignore: bool,
    on_dir: &(dyn Fn(&Path) + Sync),
) -> Result<FileTree, std::io::Error> {
    let mut root_tree = FileTree {
        name: root
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default(),
        path: "/".to_string(),
        files: Vec::new(),
        directories: Vec::new(),
    };

    let root = &root.canonicalize()?;

    let mut overrides = ignore::overrides::OverrideBuilder::new(root);
    for pattern in ALWAYS_IGNORE {
        overrides.add(pattern).unwrap();
    }
    let always_ignore = overrides.build().unwrap();

    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .standard_filters(false)
        // Prune dot-directories at the walker instead of dropping their entries
        // after the fact (below). Same outcome — a file under a hidden directory
        // never reached the tree anyway, because no directory node was created for
        // its parent — but the walker stops *descending*, so a `.snakemake` or
        // `.heudiconv` subtree costs one `readdir` rather than thousands. The test
        // is a plain filename check made before any filesystem call, and it does
        // not affect `.bidsignore`, which the walker collects from directory
        // listings rather than from the entries it yields.
        .hidden(true)
        .git_global(false)
        .git_ignore(false)
        .git_exclude(false)
        .ignore(false)
        .overrides(always_ignore)
        .threads(walk_threads());
    if apply_bidsignore {
        builder.add_custom_ignore_filename(".bidsignore");
    }
    // Do not descend *below* a pseudo-file. A `.ds`/`.ome.zarr` directory is one opaque data
    // file, so nothing inside it belongs to the tree; the serial walk used to read the whole
    // subtree and then drop every entry, because no directory node existed to hang them on.
    // The predicate tests the ancestors and not the entry itself, so the pseudo-file directory
    // is still yielded — it has to be, it becomes a `BidsFile`.
    {
        let pseudo: Vec<String> = pseudo_exts.to_vec();
        let root_owned = root.to_path_buf();
        builder.filter_entry(move |entry| !has_pseudo_ancestor(entry.path(), &root_owned, &pseudo));
    }

    // The root is a directory the walk reads too — the walker yields it at depth 0 and the
    // loop below skips it, but its `readdir` is a round trip like any other, so it is charged
    // like any other.
    //
    // What `on_dir` counts is round trips, not tree nodes: a pseudo-file directory is read
    // (that is how its contents come to be pruned) and so is charged here, while in the tree
    // it ends up a `BidsFile`. The two numbers agree for every dataset without pseudo-files.
    on_dir(root);

    // Walked in parallel, then ordered here. `ignore`'s parallel walker has no `sort_by_*` —
    // it cannot, the entries arrive from several threads — so the order the tree is built in
    // is imposed afterwards instead of inherited from the traversal.
    //
    // Sorting by path *components* rather than by the path string reproduces the serial
    // walker's sorted depth-first pre-order exactly: a parent's component list is a prefix of
    // its children's, so parents still precede them, and siblings still order by name. (A
    // plain string sort would not: `/a-b` sorts before `/a/b` because `-` < `/`, which is not
    // depth-first order and would cost `get_mut_subtree` its last-child fast path.)
    //
    // The tree that comes out is therefore the same tree, and the walk order the rest of the
    // pipeline reads is a property of the dataset rather than of how many threads ran.
    let collected: std::sync::Mutex<Vec<(PathBuf, std::fs::FileType)>> =
        std::sync::Mutex::new(Vec::new());
    let failures: std::sync::Mutex<Vec<std::io::Error>> = std::sync::Mutex::new(Vec::new());
    let failures = &failures;
    builder.build_parallel().run(|| {
        // One batch per walker thread, merged when the thread's closure is dropped. Sending
        // each entry individually — down a channel or straight into the shared `Vec` — put a
        // synchronization point on the hot path of the walk and cost more than the extra
        // threads bought.
        let mut batch = Batch {
            local: Vec::new(),
            sink: &collected,
        };
        Box::new(move |result| {
            match result {
                Ok(entry) => {
                    if let Some(ft) = entry.file_type() {
                        if ft.is_dir() && entry.depth() > 0 {
                            // Charged on the walker thread, so a per-directory cost overlaps
                            // across threads exactly as the real `readdir`s do — which is what
                            // lets a benchmark tell a parallel walk from a serial one.
                            on_dir(entry.path());
                        }
                        batch.local.push((entry.path().to_path_buf(), ft));
                    }
                }
                // The serial walk used to `unwrap()` here, so an unreadable directory aborted
                // the process. Reported instead: the signature already returns `io::Error`.
                Err(e) => failures.lock().unwrap().push(std::io::Error::other(e)),
            }
            ignore::WalkState::Continue
        })
    });

    if let Some(e) = failures.lock().unwrap().pop() {
        return Err(e);
    }
    let mut entries = collected.into_inner().unwrap();
    entries.sort_by(|(a, _), (b, _)| depth_first_order(a, b));

    for (path, file_type) in &entries {
        // although there is a min_depth option, it interacts poorly with the overrides
        // so, we explicitly ignore the root here
        if path == root {
            continue;
        }

        let entry_name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();

        // Prune dotfiles and dot-directories at every level (matches the TS validator's `.**`
        // prune rule). `.bidsignore` is still consumed internally by the walker as a custom
        // ignore source; it just isn't validated as a dataset file.
        if entry_name.starts_with('.') {
            continue;
        }

        let rel_path = make_relative_path(path, root);
        let parent_rel_path = make_relative_path(path.parent().unwrap_or(path), root);

        let parent_tree = match get_mut_subtree(&mut root_tree, &parent_rel_path) {
            Some(pt) => pt,
            None => {
                // Parent was a pseudo_file or otherwise skipped, so we skip its contents
                continue;
            }
        };

        // Classified from the type `readdir` already reported — the walker caches it on the
        // entry — rather than asking the path. Each of `Path::is_file`, `is_symlink` and
        // `is_dir` is a fresh `stat`, and on a network filesystem every one is a serialized
        // round trip for something already in hand.
        //
        // The walker runs with `follow_links(false)`, so `file_type()` reports a symlink as
        // a symlink; symlinks are bucketed with files, which is why the test order below
        // checks `is_symlink()` before `is_dir()`.
        if file_type.is_file()
            || file_type.is_symlink()
            || (file_type.is_dir() && is_pseudo_file(&entry_name, pseudo_exts))
        {
            parent_tree.files.push(BidsFile {
                name: entry_name,
                path: rel_path,
                absolute_path: path.clone(),
            });
        } else if file_type.is_dir() {
            parent_tree.directories.push(FileTree {
                name: entry_name,
                path: rel_path,
                files: Vec::new(),
                directories: Vec::new(),
            });
        }
    }

    Ok(root_tree)
}

/// Whether an entry name matches a pseudo-file extension (e.g. `.ds/`, `.ome.zarr/`) — a
/// *directory* the schema treats as one opaque data file.
///
/// Public because it is the rule by which the walk decides a directory belongs in `files`, so
/// a consumer asking "is this walked entry a directory datafile?" should ask the same question
/// of the name rather than `stat`ing the path again.
pub fn is_pseudo_file(entry_name: &str, pseudo_exts: &[String]) -> bool {
    let mut pseudo_file = false;
    for ext in pseudo_exts {
        let ext_trimmed = ext.strip_suffix('/').unwrap_or(ext);
        if !ext_trimmed.is_empty() && entry_name.ends_with(ext_trimmed) {
            pseudo_file = true;
            break;
        }
    }
    pseudo_file
}

/// Build the node for `dir` (a `/`-separated path with no leading slash; empty for the root),
/// consuming its files out of `by_dir` and recursing into its immediate subdirectories.
///
/// Immediate children come from a range scan over the sorted directory set rather than a
/// search, so the whole tree is built in one pass per level.
fn build_subtree(
    name: &str,
    dir: &str,
    dirs: &BTreeSet<String>,
    by_dir: &mut BTreeMap<String, Vec<BidsFile>>,
) -> FileTree {
    let prefix = if dir.is_empty() {
        String::new()
    } else {
        format!("{dir}/")
    };
    let children: Vec<String> = dirs
        .range(prefix.clone()..)
        .take_while(|d| d.starts_with(prefix.as_str()))
        .filter(|d| !d[prefix.len()..].contains('/'))
        .cloned()
        .collect();

    let directories = children
        .into_iter()
        .map(|child| {
            let child_name = child[prefix.len()..].to_string();
            build_subtree(&child_name, &child, dirs, by_dir)
        })
        .collect();

    FileTree {
        name: name.to_string(),
        path: if dir.is_empty() {
            "/".to_string()
        } else {
            format!("/{dir}")
        },
        files: by_dir.remove(dir).unwrap_or_default(),
        directories,
    }
}

/// Produce a `/`-separated relative path from `root` to `path`, prefixed with `/`.
fn make_relative_path(path: &Path, root: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path);
    let rel_str = rel.to_string_lossy().replace('\\', "/");
    if rel_str.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", rel_str)
    }
}

impl FileTree {
    /// Build a tree from dataset-relative paths (`sub-01/dwi/x.nii.gz`; a leading `/` is
    /// accepted and ignored).
    ///
    /// [`BidsFile::absolute_path`] is left empty, because a backend with no local files has
    /// none. That is sound only for consumers that work from `name` and `path` alone, which
    /// is exactly what association resolution does — [`find_associated_file`] and
    /// [`find_all_associated_files`] never touch it, and neither reads file content. This is
    /// what lets `bids_schema::associations` run against a backend whose listing *is* a path
    /// set (S3), rather than being restricted to backends that can produce a
    /// [`read_file_tree`].
    ///
    /// [`find_associated_file`]: crate::inheritance::find_associated_file
    /// [`find_all_associated_files`]: crate::inheritance::find_all_associated_files
    pub fn from_paths<I, S>(root_name: &str, paths: I) -> FileTree
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        // Bucket by parent directory, then build the tree top-down. Inserting each file by
        // descending from the root instead would re-scan `directories` at every level —
        // `get_mut_subtree` is a linear search per component — which is quadratic on a flat
        // root with thousands of subject directories.
        let mut by_dir: BTreeMap<String, Vec<BidsFile>> = BTreeMap::new();
        let mut dirs: BTreeSet<String> = BTreeSet::new();

        for p in paths {
            let rel = p.as_ref().trim_start_matches('/');
            if rel.is_empty() {
                continue;
            }
            let (dir, name) = match rel.rfind('/') {
                Some(i) => (&rel[..i], &rel[i + 1..]),
                None => ("", rel),
            };
            by_dir.entry(dir.to_string()).or_default().push(BidsFile {
                name: name.to_string(),
                path: format!("/{rel}"),
                absolute_path: PathBuf::new(),
            });
            // Every ancestor needs a node, including ones holding no files of their own.
            let mut ancestor = String::new();
            for component in dir.split('/').filter(|c| !c.is_empty()) {
                if !ancestor.is_empty() {
                    ancestor.push('/');
                }
                ancestor.push_str(component);
                dirs.insert(ancestor.clone());
            }
        }

        // Match `read_file_tree`'s `sort_by_file_name`: `find_associated_file` returns the
        // first hit in a directory, so a stable order is what makes the resolution
        // deterministic rather than dependent on the order paths arrived in.
        for files in by_dir.values_mut() {
            files.sort_by(|a, b| a.name.cmp(&b.name));
        }

        build_subtree(root_name, "", &dirs, &mut by_dir)
    }

    /// Recursively iterate over all files in this tree.
    /// Return an iterator over all files in the tree, traversing recursively.
    pub fn walk_files(&self) -> WalkFiles<'_> {
        WalkFiles {
            stack: vec![self],
            current_files: None,
        }
    }

    /// Return an iterator over all directories in the tree, traversing recursively.
    pub fn walk_directories(&self) -> WalkDirectories<'_> {
        WalkDirectories { stack: vec![self] }
    }

    /// Find a file by its relative path.
    /// Search the tree for a file with the given relative path.
    pub fn find_file(&self, rel_path: &str) -> Option<&BidsFile> {
        for file in &self.files {
            if file.path == rel_path {
                return Some(file);
            }
        }
        for dir in &self.directories {
            if let Some(f) = dir.find_file(rel_path) {
                return Some(f);
            }
        }
        None
    }

    /// Find a subdirectory by name (immediate children only).
    /// Search the current directory's direct subdirectories for a given name.
    pub fn find_dir(&self, name: &str) -> Option<&FileTree> {
        self.directories.iter().find(|d| d.name == name)
    }

    /// Get the file tree at a relative path (e.g. "/sub-01/anat").
    /// Get a subtree (directory) by its relative path from the dataset root.
    pub fn subtree(&self, rel_path: &str) -> Option<&FileTree> {
        let parts: Vec<&str> = rel_path
            .strip_prefix('/')
            .unwrap_or(rel_path)
            .split('/')
            .filter(|s| !s.is_empty())
            .collect();

        let mut current = self;
        for part in parts {
            {
                let d = current.find_dir(part)?;
                current = d
            }
        }
        Some(current)
    }

    /// Check if a file exists in this tree by relative path.
    pub fn file_exists(&self, rel_path: &str) -> bool {
        self.find_file(rel_path).is_some()
    }
}

/// Every [`BidsFile`] in a tree, depth-first: the files a directory holds directly, then
/// each subdirectory in turn (see [`FileTree::walk_files`]).
///
/// The order is deterministic and both constructors make it alphabetical —
/// [`read_file_tree`] sorts the walk by filename and [`FileTree::from_paths`] sorts each
/// directory's files — which is what makes "the first match wins" resolutions like
/// [`find_associated_file`] repeatable rather than dependent on `readdir` order.
///
/// In a tree from [`read_file_tree`], a pseudo-file directory (`.ds/`, `.ome.zarr/`; see
/// [`is_pseudo_file`]) arrives here as a single file and its contents never do — so this
/// yields the dataset's *entries*, which is not the same set as its POSIX files.
///
/// [`find_associated_file`]: crate::inheritance::find_associated_file
#[derive(Debug)]
pub struct WalkFiles<'a> {
    stack: Vec<&'a FileTree>,
    current_files: Option<std::slice::Iter<'a, BidsFile>>,
}

impl<'a> Iterator for WalkFiles<'a> {
    type Item = &'a BidsFile;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(files) = &mut self.current_files
                && let Some(file) = files.next()
            {
                return Some(file);
            }

            {
                let current_dir = self.stack.pop()?;
                self.current_files = Some(current_dir.files.iter());
                for dir in current_dir.directories.iter().rev() {
                    self.stack.push(dir);
                }
            }
        }
    }
}

/// Every directory node in a tree, depth-first (see [`FileTree::walk_directories`]).
///
/// **Includes the node it was started from.** `tree.walk_directories().count()` on a dataset
/// root is therefore one more than the number of subdirectories it contains, and an empty tree
/// still yields one item rather than none.
#[derive(Debug)]
pub struct WalkDirectories<'a> {
    stack: Vec<&'a FileTree>,
}

impl<'a> Iterator for WalkDirectories<'a> {
    type Item = &'a FileTree;

    fn next(&mut self) -> Option<Self::Item> {
        let current_dir = self.stack.pop()?;
        for dir in current_dir.directories.iter().rev() {
            self.stack.push(dir);
        }
        Some(current_dir)
    }
}

impl BidsFile {
    /// The file's size in bytes, `stat`-ed on demand.
    ///
    /// Deliberately not a field: filling one would mean a `stat` on every entry of
    /// the walk, and only the validator ever asks — once per file, while building a
    /// `BidsContext`. Ingestion never reads it, so paying for it during the walk
    /// charged every consumer for one consumer's need.
    ///
    /// Follows symlinks (a symlink reports its target's size) and yields 0 for
    /// anything that cannot be stat-ed, matching what the field held for a broken
    /// symlink.
    pub fn size_bytes(&self) -> u64 {
        std::fs::metadata(&self.absolute_path)
            .map(|m| m.len())
            .unwrap_or(0)
    }

    /// Read the full contents of this file as bytes.
    pub async fn read_bytes(&self) -> Result<Vec<u8>, std::io::Error> {
        return tokio::fs::read(&self.absolute_path).await;
    }

    /// Read the full contents of this file as a string.
    pub async fn read_string(&self) -> Result<String, std::io::Error> {
        return tokio::fs::read_to_string(&self.absolute_path).await;
    }

    /// Get the parent directory path (relative to dataset root).
    pub fn parent_path(&self) -> &str {
        self.path.rfind('/').map(|i| &self.path[..i]).unwrap_or("/")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pseudo-file rule is now public, because consumers decide "is this walked entry a
    /// directory datafile?" from the name rather than by `stat`ing the path — so what it
    /// accepts is a contract, not an implementation detail. Notably it keys on the *name*:
    /// a symlink to an ordinary directory is not a pseudo-file, and a schema extension is
    /// matched with or without its trailing slash.
    #[test]
    fn pseudo_file_is_decided_by_name() {
        // Exactly what BIDS 1.11.1 yields for `pseudo_file_extensions`, bare `/` included:
        // it strips to the empty string, which must not then match every name.
        let exts: Vec<String> = [".ds/", ".mefd/", ".ome.zarr/", "/"]
            .iter()
            .map(|s| s.to_string())
            .collect();

        for name in [
            "sub-01_task-x_meg.ds",
            "sub-01_sample-a_image.ome.zarr",
            "sub-01_ieeg.mefd",
        ] {
            assert!(
                is_pseudo_file(name, &exts),
                "{name} should be a pseudo-file"
            );
        }
        for name in [
            "sub-01_T1w.nii.gz",
            "anat",
            "some_linked_directory",
            "meg", // the datatype directory itself, not a `.ds` bundle
            "",
        ] {
            assert!(!is_pseudo_file(name, &exts), "{name} should not be");
        }
        // An empty extension must never match everything.
        assert!(!is_pseudo_file(
            "anything",
            &["".to_string(), "/".to_string()]
        ));
    }

    #[test]
    fn test_make_relative_path() {
        let root = Path::new("/data/mybids");
        let path = Path::new("/data/mybids/sub-01/anat/sub-01_T1w.nii.gz");
        assert_eq!(
            make_relative_path(path, root),
            "/sub-01/anat/sub-01_T1w.nii.gz"
        );
    }

    #[test]
    fn test_make_relative_path_root() {
        let root = Path::new("/data/mybids");
        assert_eq!(make_relative_path(root, root), "/");
    }

    #[test]
    fn test_bidsfile_parent_path() {
        let f = BidsFile {
            name: "sub-01_T1w.nii.gz".into(),
            path: "/sub-01/anat/sub-01_T1w.nii.gz".into(),
            absolute_path: PathBuf::from("/data/mybids/sub-01/anat/sub-01_T1w.nii.gz"),
        };
        assert_eq!(f.parent_path(), "/sub-01/anat");
    }

    #[test]
    fn test_always_ignore_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        // Create directories that should be ignored (matching ALWAYS_IGNORE_DIRS)
        for pattern in ALWAYS_IGNORE {
            let dir_name = pattern.strip_prefix('!').unwrap_or(pattern);
            let dir = root.join(dir_name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("config"), "ignored").unwrap();
        }

        // Create a normal directory that should be included
        let sub = root.join("sub-01");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("sub-01_T1w.nii.gz"), "data").unwrap();

        let tree = read_file_tree(root, &[], true).unwrap();

        // The always-ignored directories must not appear
        for pattern in ALWAYS_IGNORE {
            let dir_name = pattern.strip_prefix('!').unwrap_or(pattern);
            assert!(
                tree.find_dir(dir_name).is_none(),
                "directory '{}' should have been ignored",
                dir_name,
            );
            // Their contents must also be absent
            let file_path = format!("/{}/config", dir_name);
            assert!(
                tree.find_file(&file_path).is_none(),
                "file '{}' inside ignored dir should not appear",
                file_path,
            );
        }

        // The normal directory and its file must be present
        assert!(tree.find_dir("sub-01").is_some());
        assert!(tree.find_file("/sub-01/sub-01_T1w.nii.gz").is_some());
    }

    /// `from_paths` must produce the same shape `read_file_tree` does — `/`-prefixed paths,
    /// a node per ancestor directory, root-level files on the root — because the association
    /// resolver walks it by `subtree(dir)` and compares `file.path`.
    #[test]
    fn from_paths_builds_the_same_shape_as_a_walk() {
        let tree = FileTree::from_paths(
            "ds114",
            [
                // Deliberately unsorted, and mixing leading-slash conventions: the S3 listing
                // and the file registry each have their own, and neither is guaranteed sorted.
                "sub-01/ses-test/dwi/sub-01_ses-test_dwi.nii.gz",
                "/dwi.bval",
                "dwi.bvec",
                "sub-01/ses-test/anat/sub-01_ses-test_T1w.nii.gz",
            ],
        );

        // Root-level files land on the root, which is what makes an inherited gradient
        // reachable from a subject directory three levels down.
        assert_eq!(tree.path, "/");
        assert_eq!(tree.name, "ds114");
        let root_names: Vec<&str> = tree.files.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(root_names, vec!["dwi.bval", "dwi.bvec"]); // sorted, like the walk

        // Every ancestor exists as a node, including `ses-test`, which holds no files itself.
        assert!(tree.subtree("/sub-01/ses-test").is_some());
        let dwi = tree.subtree("/sub-01/ses-test/dwi").expect("dwi dir");
        assert_eq!(dwi.name, "dwi");
        assert_eq!(dwi.files.len(), 1);
        assert_eq!(
            dwi.files[0].path,
            "/sub-01/ses-test/dwi/sub-01_ses-test_dwi.nii.gz"
        );

        // A leading slash on the input is accepted, not doubled.
        assert!(tree.find_file("/dwi.bval").is_some());
        assert!(tree.find_file("//dwi.bval").is_none());

        // Round-trip: every input path comes back out of `walk_files`.
        let mut walked: Vec<&str> = tree.walk_files().map(|f| f.path.as_str()).collect();
        walked.sort();
        assert_eq!(
            walked,
            vec![
                "/dwi.bval",
                "/dwi.bvec",
                "/sub-01/ses-test/anat/sub-01_ses-test_T1w.nii.gz",
                "/sub-01/ses-test/dwi/sub-01_ses-test_dwi.nii.gz",
            ]
        );
    }

    /// `absolute_path` is deliberately empty — a backend with no local files has none. Pinned
    /// so that a consumer added later cannot start depending on it without this test failing.
    #[test]
    fn from_paths_leaves_absolute_path_empty() {
        let tree = FileTree::from_paths("ds", ["sub-01/anat/sub-01_T1w.nii.gz"]);
        let file = tree.walk_files().next().expect("one file");
        assert_eq!(file.absolute_path, PathBuf::new());
    }

    #[test]
    fn from_paths_tolerates_empty_and_slash_only_entries() {
        let tree = FileTree::from_paths("ds", ["", "/", "sub-01/anat/sub-01_T1w.nii.gz"]);
        assert_eq!(tree.walk_files().count(), 1);
    }
}
