//! Schema-driven structural associations (via `bids_schema::associations`) — in particular that
//! they source from **all** data files, not just NIfTI imaging files.

mod common;

use common::{bids_example, ingest};

/// A non-NIfTI EEG source (`_eeg.vhdr`) resolves its sibling `channels.tsv` through the schema's
/// `meta.associations`. This proves the resolver iterates every data file in the tree (the EEG
/// raw file is not a NIfTI, so the old `imaging_files`-only path would have missed it).
#[tokio::test]
async fn channels_association_from_non_nifti_eeg_source() -> anyhow::Result<()> {
    let db = ingest(bids_example("eeg_matchingpennies")).await?;

    let channels: i64 = db.conn.query_row(
        "SELECT COUNT(*) FROM file_associations \
         WHERE association_type = 'channels' \
           AND source_file_path LIKE '%_eeg.vhdr' \
           AND target_file_path LIKE '%_channels.tsv'",
        [],
        |r| r.get(0),
    )?;
    assert!(
        channels >= 1,
        "an EEG source should resolve a `channels` association; got {channels}"
    );
    Ok(())
}

/// A MEG **pseudo-file** source (`_meg.ds` — a directory BIDS treats as one file) resolves its
/// sibling `channels.tsv`. This proves pseudo-files are emitted as files (and thus association
/// sources), which requires the schema-driven `pseudo_file_extensions` in the walk (E4b).
#[tokio::test]
async fn channels_association_from_meg_pseudo_file() -> anyhow::Result<()> {
    let db = ingest(bids_example("ds000246")).await?;

    let channels: i64 = db.conn.query_row(
        "SELECT COUNT(*) FROM file_associations \
         WHERE association_type = 'channels' \
           AND source_file_path LIKE '%_meg.ds' \
           AND target_file_path LIKE '%_channels.tsv'",
        [],
        |r| r.get(0),
    )?;
    assert!(
        channels >= 1,
        "a MEG `.ds` pseudo-file should resolve a `channels` association; got {channels}"
    );
    Ok(())
}
