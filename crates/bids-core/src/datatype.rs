//! Where a BIDS path says a file's datatype is: its **immediate** parent directory.
//!
//! The rule is schema-agnostic and lives here; the caller supplies the datatype set, the same
//! way [`crate::entities::resolve_entities`] takes the entity map rather than reading a schema.
//! `bids-schema`, the crate that owns the schema JSON, reads that set out of it once
//! (`SchemaIndex`) and then answers here — so the rule has one spelling and the schema is
//! walked once per schema rather than once per file.

use std::collections::HashSet;

/// The name of a path's immediate parent directory, or `None` when the path has fewer than two
/// non-empty segments (`/participants.tsv`, `README`, `""`).
///
/// Leading, trailing and repeated separators are ignored, so `/sub-01/anat/x.nii.gz` and
/// `sub-01/anat/x.nii.gz` agree.
///
/// # Examples
/// ```
/// use bids_core::datatype::parent_dir;
///
/// assert_eq!(parent_dir("/sub-01/anat/sub-01_T1w.nii.gz"), Some("anat"));
/// assert_eq!(parent_dir("/participants.tsv"), None);
/// ```
pub fn parent_dir(path: &str) -> Option<&str> {
    let mut segments = path.rsplit('/').filter(|s| !s.is_empty());
    segments.next()?; // the file itself
    segments.next()
}

/// The datatype a BIDS-named file gets from its position: its immediate parent directory, when
/// that directory names a known datatype.
///
/// `datatypes` is the schema's datatype set (`bids_schema::datatypes::datatypes`), derived once
/// per schema rather than re-walked out of the schema JSON for every file. Borrowed from `path`,
/// so a hot loop pays no allocation.
///
/// # Examples
/// ```
/// use bids_core::datatype::parent_datatype;
/// use std::collections::HashSet;
///
/// let datatypes: HashSet<String> = ["anat", "func"].iter().map(|s| s.to_string()).collect();
/// assert_eq!(parent_datatype("/sub-01/anat/sub-01_T1w.nii.gz", &datatypes), Some("anat"));
/// // A datatype that is not the *immediate* parent does not count.
/// assert_eq!(parent_datatype("/sub-01/anat/extra/nested.nii.gz", &datatypes), None);
/// ```
pub fn parent_datatype<'a>(path: &'a str, datatypes: &HashSet<String>) -> Option<&'a str> {
    parent_dir(path).filter(|parent| datatypes.contains(*parent))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::strategy;
    use proptest::prelude::*;
    use rstest::rstest;

    fn datatypes() -> HashSet<String> {
        ["anat", "func", "eeg", "meg", "dwi"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    proptest! {
        /// The parent is the second-to-last non-empty segment, or `None` when there are fewer
        /// than two of them.
        ///
        /// Replaces the eight cases this rule used to be judged on (absolute, relative,
        /// doubled separators, subject dir, bare filename, root file, `/`, `""`). Each was one
        /// point of this law, and between them they doubled a separator at exactly one
        /// position; the generator varies leading, trailing and interior runs of 1–3 `/`
        /// independently, over 0–5 segments, so the "fewer than two" boundary is crossed from
        /// both sides every run.
        #[test]
        fn parent_dir_is_the_second_to_last_non_empty_segment(
            (path, segments) in strategy::slashed_path(0..=5)
        ) {
            let parent = parent_dir(&path);

            prop_assert_eq!(
                parent,
                segments.len().checked_sub(2).map(|i| segments[i].as_str())
            );
        }
    }

    /// The path set this rule is judged on, carried over from bidslake, which had its own copy
    /// of the rule and a test pinning it against bids-schema's. Both now call this.
    ///
    /// Deliberately *not* a property: the only law here is "the parent dir, when it is in the
    /// set", which is `parent_datatype`'s body verbatim — a generated version would assert the
    /// implementation against itself. What the cases are for is the rule's edges, so they are
    /// trimmed to those: the four positive rows for anat/func/eeg/meg all demonstrated the same
    /// lookup, and the two `None` rows for too-few-segments belong to `parent_dir`, whose
    /// property above now covers them over every shape.
    #[rstest]
    #[case::datatype_parent("sub-01/ses-1/eeg/sub-01_ses-1_task-x_eeg.vhdr", Some("eeg"))]
    // Position is the whole rule: a datatype directory at the dataset root still counts...
    #[case::root_level_datatype("anat/loose.nii.gz", Some("anat"))]
    // ...and a datatype that is not the *immediate* parent does not.
    #[case::not_immediate_parent("sub-01/anat/extra/nested.nii.gz", None)]
    // A parent that exists but names no datatype.
    #[case::subject_dir("sub-01/sub-01_scans.tsv", None)]
    fn parent_datatype_over_the_corpus_shapes(#[case] path: &str, #[case] expected: Option<&str>) {
        assert_eq!(parent_datatype(path, &datatypes()), expected);
    }
}
