//! `bidslake verify` — is the catalog still true about the files it names?
//!
//! A catalog is an index, never an owner (ADR 0005), so the tree underneath it moves
//! without asking. This asks the only two questions an index can answer about that: is
//! every file it recorded still there, and does it still look the way it did.
//!
//! **What "look the way it did" means is bounded by what the ingest stored**, and that is
//! deliberately size and mtime rather than a checksum. Hashing means *reading* every file,
//! which is a different order of cost from stat-ing one, and an index that read every byte
//! of a study to build itself would not be an index. So this detects a file that was
//! replaced, truncated, or rewritten — the failures that actually happen to a derivative
//! tree — and cannot detect a rewrite that preserved both. That is the same bargain `make`
//! has always made.
//!
//! A catalog indexed with `--no-stat` has neither column, and then this degrades to
//! presence alone and says so, rather than reporting a clean bill it cannot support.

use anyhow::{Context as _, Result};
use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::db::BidsDb;
use crate::fs::{BidsFileSystem, FileStat, LocalFileSystem};

/// What verification found about one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Finding {
    /// Present, and matching the size and mtime recorded at index time.
    Ok,
    /// Present, but the catalog stored no stat to compare against (`--no-stat`).
    PresentUnchecked,
    /// The catalog names it; the filesystem does not have it.
    Missing,
    /// Present, but its size or mtime has moved since it was indexed.
    Changed,
}

/// `(file_path, the stat recorded at index time)` — the stat absent under `--no-stat`.
type RecordedFile = (String, Option<FileStat>);

/// One dataset root's worth of rows to check.
struct Rooted {
    dataset_id: String,
    root_uri: String,
    files: Vec<RecordedFile>,
}

/// Read the registry, grouped by the root each path is relative to.
///
/// Grouped because `file_path` means nothing on its own: a dataset may span many roots
/// (subject-sharded pipeline output is the normal case), and two roots can hold the same
/// relative path. Resolution has to go through `dataset_roots`, which is what `file_id`
/// already keys on.
fn rows_by_root(db: &BidsDb) -> Result<Vec<Rooted>> {
    let mut stmt = db
        .conn
        .prepare(
            "SELECT dataset_id, root_uri, file_path, size_bytes, mtime_ns \
             FROM file_registry ORDER BY dataset_id, root_uri, file_path",
        )
        .context("reading the file registry")?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<u64>>(3)?,
                r.get::<_, Option<i64>>(4)?,
            ))
        })
        .context("reading the file registry")?;

    let mut grouped: BTreeMap<(String, String), Vec<RecordedFile>> = BTreeMap::new();
    for row in rows {
        let (dataset_id, root_uri, file_path, size, mtime) = row.context("a registry row")?;
        let stat = match (size, mtime) {
            (Some(size_bytes), Some(mtime_ns)) => Some(FileStat {
                size_bytes,
                mtime_ns,
            }),
            _ => None,
        };
        grouped
            .entry((dataset_id, root_uri))
            .or_default()
            .push((file_path, stat));
    }
    Ok(grouped
        .into_iter()
        .map(|((dataset_id, root_uri), files)| Rooted {
            dataset_id,
            root_uri,
            files,
        })
        .collect())
}

/// A `file://` root as a local path, or `None` for a root this build cannot reach.
fn local_root(root_uri: &str) -> Option<PathBuf> {
    root_uri
        .strip_prefix("file://")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
}

/// Verify every file in `database`, printing a report. Returns the number of problems.
pub async fn run(database: &str) -> Result<usize> {
    let db = BidsDb::new(database).with_context(|| format!("opening the catalog at {database}"))?;
    let roots = rows_by_root(&db)?;
    if roots.is_empty() {
        println!("catalog holds no files");
        return Ok(0);
    }

    let mut totals: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut problems = 0usize;

    for rooted in &roots {
        let Some(root) = local_root(&rooted.root_uri) else {
            // An `s3://` root, or a build without the feature to reach it. Skipped loudly:
            // silently counting it verified would be the one outcome worse than not
            // checking.
            println!(
                "{}: skipping {} — only local roots can be verified by this build",
                rooted.dataset_id, rooted.root_uri
            );
            *totals.entry("skipped").or_default() += rooted.files.len();
            continue;
        };

        let fs = LocalFileSystem::new(&root);
        let paths: Vec<PathBuf> = rooted.files.iter().map(|(p, _)| PathBuf::from(p)).collect();
        // The same concurrent stat the ingest uses, for the same reason: on a parallel
        // filesystem this is a million round trips, and serially that is the difference
        // between a check somebody runs and one they do not.
        let seen = fs.stat_many(&paths).await?;

        for ((file_path, recorded), found) in rooted.files.iter().zip(seen) {
            let finding = match (recorded, found) {
                (_, None) => Finding::Missing,
                (None, Some(_)) => Finding::PresentUnchecked,
                (Some(was), Some(now)) if *was == now => Finding::Ok,
                (Some(_), Some(_)) => Finding::Changed,
            };
            match finding {
                Finding::Ok => *totals.entry("ok").or_default() += 1,
                Finding::PresentUnchecked => *totals.entry("present").or_default() += 1,
                Finding::Missing => {
                    *totals.entry("missing").or_default() += 1;
                    problems += 1;
                    println!("missing  {}  {file_path}", rooted.dataset_id);
                }
                Finding::Changed => {
                    *totals.entry("changed").or_default() += 1;
                    problems += 1;
                    println!("changed  {}  {file_path}", rooted.dataset_id);
                }
            }
        }
    }

    let line = ["ok", "present", "changed", "missing", "skipped"]
        .iter()
        .filter_map(|k| totals.get(k).map(|n| format!("{n} {k}")))
        .collect::<Vec<_>>()
        .join(", ");
    println!("{line}");
    if totals.contains_key("present") {
        println!(
            "note: {} file(s) were checked for presence only — this catalog was indexed \
             with --no-stat, so it stored no size or mtime to compare against",
            totals["present"]
        );
    }
    Ok(problems)
}
