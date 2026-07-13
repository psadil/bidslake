//! File tree representation and directory walking.
//!
//! Reads a directory on disk into an in-memory tree of files and directories,
//! respecting `.bidsignore` patterns.

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
    /// File size in bytes.
    pub size: u64,
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
        let idx = current.directories.iter().position(|d| d.name == part)?;
        current = &mut current.directories[idx];
    }
    Some(current)
}

/// Read a directory into a `FileTree`, respecting `.bidsignore` patterns.
///
/// The `root` path should point to the top of the BIDS dataset (the directory
/// containing `dataset_description.json`).
pub fn read_file_tree(root: &Path, pseudo_exts: &[String]) -> Result<FileTree, std::io::Error> {
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
        .add_custom_ignore_filename(".bidsignore")
        .hidden(false)
        .git_global(false)
        .git_ignore(false)
        .git_exclude(false)
        .ignore(false)
        .overrides(always_ignore)
        .sort_by_file_name(|a, b| a.cmp(b));

    let walker = builder.build();

    for result in walker {
        let entry = result.unwrap();
        // although there is a min_depth option, it interacts poorly with the overrides
        // so, we explicitly ignore the root here
        if entry.depth() == 0 {
            continue;
        }
        let path = entry.path();

        let entry_name = entry.file_name().to_string_lossy().to_string();

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

        if entry.path().is_file()
            || entry.path().is_symlink()
            || (entry.path().is_dir() && is_pseudo_file(&entry_name, pseudo_exts))
        {
            let size = match std::fs::metadata(entry.path()) {
                Ok(metadata) => metadata.len(),
                Err(_) => 0, // Default to 0 for broken symlinks
            };
            parent_tree.files.push(BidsFile {
                name: entry_name,
                path: rel_path,
                absolute_path: entry.path().to_path_buf(),
                size,
            });
        } else if entry.path().is_dir() {
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

// Check if directory matches a pseudo-file extension (e.g. ".ds/")
fn is_pseudo_file(entry_name: &str, pseudo_exts: &[String]) -> bool {
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
            size: 1000,
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

        let tree = read_file_tree(root, &[]).unwrap();

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
}
