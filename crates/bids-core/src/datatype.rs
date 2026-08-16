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
    use rstest::rstest;

    fn datatypes() -> HashSet<String> {
        ["anat", "func", "eeg", "meg", "dwi"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    #[rstest]
    #[case::absolute("/sub-01/anat/sub-01_T1w.nii.gz", Some("anat"))]
    #[case::relative("sub-01/anat/sub-01_T1w.nii.gz", Some("anat"))]
    #[case::empty_segments("sub-01//anat//x.nii.gz", Some("anat"))]
    #[case::subject_dir("/sub-01/sub-01_scans.tsv", Some("sub-01"))]
    // Too few segments to have a parent at all.
    #[case::bare_filename("dataset_description.json", None)]
    #[case::root_file("/README", None)]
    #[case::root("/", None)]
    #[case::empty("", None)]
    fn parent_dir_is_the_second_to_last_non_empty_segment(
        #[case] path: &str,
        #[case] expected: Option<&str>,
    ) {
        assert_eq!(parent_dir(path), expected);
    }

    /// The path set this rule is judged on, carried over from bidslake, which had its own copy
    /// of the rule and a test pinning it against bids-schema's. Both now call this.
    #[rstest]
    #[case("sub-01/anat/sub-01_T1w.nii.gz", Some("anat"))]
    #[case("sub-01/func/sub-01_task-rest_bold.nii.gz", Some("func"))]
    #[case("sub-01/ses-1/eeg/sub-01_ses-1_task-x_eeg.vhdr", Some("eeg"))]
    #[case("sub-01/meg/sub-01_task-x_meg.ds", Some("meg"))]
    #[case(
        "derivatives/fmriprep/sub-01/anat/sub-01_desc-preproc_T1w.nii.gz",
        Some("anat")
    )]
    // Position is the whole rule: a datatype directory at the dataset root still counts...
    #[case::root_level_datatype("anat/loose.nii.gz", Some("anat"))]
    // ...and a datatype that is not the *immediate* parent does not.
    #[case::not_immediate_parent("sub-01/anat/extra/nested.nii.gz", None)]
    // Not a datatype directory, or too few segments to have a parent at all.
    #[case::subject_dir("sub-01/sub-01_scans.tsv", None)]
    #[case::dataset_root("dataset_description.json", None)]
    #[case::no_parent("README", None)]
    fn parent_datatype_over_the_corpus_shapes(#[case] path: &str, #[case] expected: Option<&str>) {
        assert_eq!(parent_datatype(path, &datatypes()), expected);
    }
}
