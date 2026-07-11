//! `scans` holds a row per primary data file across modalities (not just NIfTI), so
//! electrophysiology/pseudo-file datafiles are queryable by concept.

mod common;

use common::{bids_example, ingest};

/// A non-NIfTI EEG datafile (BrainVision `.vhdr`) and a MEG **pseudo-file** (`.ds`) both land in
/// `scans`, queryable by their derived `datatype`/`suffix` concept columns.
#[tokio::test]
async fn scans_includes_non_nifti_and_pseudo_datafiles() -> anyhow::Result<()> {
    let eeg = ingest(bids_example("eeg_matchingpennies")).await?;
    let n_eeg: i64 = eeg.conn.query_row(
        "SELECT COUNT(*) FROM scans \
         WHERE datatype = 'eeg' AND suffix = 'eeg' AND file_path LIKE '%_eeg.vhdr'",
        [],
        |r| r.get(0),
    )?;
    assert!(
        n_eeg >= 1,
        "an EEG .vhdr datafile should be in scans; got {n_eeg}"
    );

    let meg = ingest(bids_example("ds000246")).await?;
    let n_meg: i64 = meg.conn.query_row(
        "SELECT COUNT(*) FROM scans \
         WHERE datatype = 'meg' AND suffix = 'meg' AND file_path LIKE '%_meg.ds'",
        [],
        |r| r.get(0),
    )?;
    assert!(
        n_meg >= 1,
        "a MEG .ds pseudo-file datafile should be in scans; got {n_meg}"
    );
    Ok(())
}

/// Pseudo-files carry the `pseudofile` marker column, their internal components are **not**
/// indexed (tools operate at the directory level), and regular files are `pseudofile = false`.
#[tokio::test]
async fn pseudofile_column_and_no_components() -> anyhow::Result<()> {
    let meg = ingest(bids_example("ds000246")).await?;

    // The `.ds` MEG directory is one scan, marked `pseudofile = true`.
    let ds_pseudo: i64 = meg.conn.query_row(
        "SELECT COUNT(*) FROM scans WHERE file_path LIKE '%_meg.ds' AND pseudofile",
        [],
        |r| r.get(0),
    )?;
    assert!(
        ds_pseudo >= 1,
        "a `_meg.ds` scan should have pseudofile = true; got {ds_pseudo}"
    );

    // No component *inside* a `.ds` directory is indexed.
    let components: i64 = meg.conn.query_row(
        "SELECT COUNT(*) FROM scans WHERE file_path LIKE '%.ds/%'",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(
        components, 0,
        "pseudo-file components must not be indexed; got {components}"
    );

    // A regular NIfTI scan is not a pseudo-file.
    let mri = ingest(bids_example("ds001")).await?;
    let nii_pseudo: i64 = mri.conn.query_row(
        "SELECT COUNT(*) FROM scans WHERE suffix = 'bold' AND pseudofile",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(
        nii_pseudo, 0,
        "NIfTI scans must have pseudofile = false; got {nii_pseudo}"
    );
    Ok(())
}
