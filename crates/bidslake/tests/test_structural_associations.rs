//! Schema-driven structural associations (via `bids_schema::associations`) — in particular that
//! they source from **all** data files, not just NIfTI imaging files, and that an *inherited*
//! target several directory levels up resolves.

mod common;

use common::{bids_example, ingest};
use rstest::rstest;

/// A non-NIfTI EEG source (`_eeg.vhdr`) resolves its sibling `channels.tsv` through the schema's
/// `meta.associations`. This proves the resolver iterates every data file in the tree (the EEG
/// raw file is not a NIfTI, so the old `imaging_files`-only path would have missed it).
#[tokio::test]
async fn channels_association_from_non_nifti_eeg_source() -> anyhow::Result<()> {
    let db = ingest(bids_example("eeg_matchingpennies")).await?;

    let channels: i64 = db.conn.query_row(
        "SELECT COUNT(*) FROM file_associations a \
         JOIN all_files src ON src.file_id = a.source_file_id \
         WHERE a.association_type = 'channels' \
           AND src.file_path LIKE '%_eeg.vhdr' \
           AND a.target_file_path LIKE '%_channels.tsv'",
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
        "SELECT COUNT(*) FROM file_associations a \
         JOIN all_files src ON src.file_id = a.source_file_id \
         WHERE a.association_type = 'channels' \
           AND src.file_path LIKE '%_meg.ds' \
           AND a.target_file_path LIKE '%_channels.tsv'",
        [],
        |r| r.get(0),
    )?;
    assert!(
        channels >= 1,
        "a MEG `.ds` pseudo-file should resolve a `channels` association; got {channels}"
    );
    Ok(())
}

/// The case the whole `diffusion` rework rests on: `ds114` ships **one** `dwi.bval`/`dwi.bvec`
/// pair at its root, and every `sub-XX/ses-YY/dwi/*_dwi.nii.gz` three levels below inherits it.
///
/// `find_associated_file` gets there because `meta.associations.bval` sets `inherit: true` (so the
/// search ascends to the root) and declares **no** `target.suffix` (so the target suffix defaults
/// to the source's, `dwi`, which the entity-less `dwi.bval` matches). Nothing but this test says
/// the edges exist — they were being resolved and then ignored before `bvals`/`bvecs` consumed
/// them.
#[rstest]
#[case("bval")]
#[case("bvec")]
#[tokio::test]
async fn inherited_gradients_resolve_to_every_image_below(
    #[case] kind: &str,
) -> anyhow::Result<()> {
    let db = ingest(bids_example("ds114")).await?;

    let (n, targets): (i64, i64) = db.conn.query_row(
        "SELECT COUNT(*), COUNT(DISTINCT target_file_path) FROM file_associations \
         WHERE association_type = ?",
        [kind],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;

    // 10 subjects x 2 sessions, all pointing at the single root-level file.
    assert_eq!(
        (n, targets),
        (20, 1),
        "{kind}: one edge per image, all sharing the one inherited target"
    );
    Ok(())
}

/// `target_file_id` must be resolved, not NULL: the gradient file is registered under its own
/// path, so nothing here is a dangling reference.
#[rstest]
#[case("bval")]
#[case("bvec")]
#[tokio::test]
async fn an_inherited_gradient_target_resolves_to_an_id(#[case] kind: &str) -> anyhow::Result<()> {
    let db = ingest(bids_example("ds114")).await?;

    let dangling: i64 = db.conn.query_row(
        "SELECT COUNT(*) FROM file_associations \
         WHERE association_type = ? AND target_file_id IS NULL",
        [kind],
        |r| r.get(0),
    )?;

    assert_eq!(
        dangling, 0,
        "{kind}: inherited target should resolve to an id"
    );
    Ok(())
}

/// The sources are the images themselves, addressable by concept through `all_files`.
#[tokio::test]
async fn inherited_gradient_sources_are_the_images_themselves() -> anyhow::Result<()> {
    let db = ingest(bids_example("ds114")).await?;

    let sessions: i64 = db.conn.query_row(
        "SELECT COUNT(DISTINCT (f.sub, f.ses)) FROM file_associations a \
         JOIN all_files f ON f.file_id = a.source_file_id \
         WHERE a.association_type = 'bval'",
        [],
        |r| r.get(0),
    )?;

    assert_eq!(sessions, 20);
    Ok(())
}

/// The same inherited shape in a second dataset, so the fix is not ds114-shaped: `genetics_ukbb`
/// has a root gradient pair and no sessions.
#[tokio::test]
async fn inherited_gradients_resolve_without_sessions() -> anyhow::Result<()> {
    let db = ingest(bids_example("genetics_ukbb")).await?;
    let n: i64 = db.conn.query_row(
        "SELECT COUNT(*) FROM file_associations WHERE association_type = 'bval'",
        [],
        |r| r.get(0),
    )?;
    assert!(
        n > 0,
        "a root-level dwi.bval should reach the images below it"
    );
    Ok(())
}

/// `resolve_structural_associations` builds its tree from the **registry path set** rather than
/// the backend's walked `FileTree`, which is what lets it run on S3 (docs/adr/0003). The two must
/// denote the same files, so the edge set must be identical to what the walk produces.
///
/// Asserted indirectly but exactly: every association's source and target are registry rows, and
/// the counts on a dataset with the full variety of association kinds are pinned. If the two
/// sets ever diverge, an edge loses its `target_file_id` or disappears.
#[tokio::test]
async fn every_association_endpoint_is_a_registry_row() -> anyhow::Result<()> {
    let db = ingest(bids_example("ds000117")).await?;

    let orphan_sources: i64 = db.conn.query_row(
        "SELECT COUNT(*) FROM file_associations a \
         LEFT JOIN file_registry f ON f.file_id = a.source_file_id \
         WHERE f.file_id IS NULL",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(
        orphan_sources, 0,
        "an association source is always a walked file"
    );

    // A structural target is resolved from the same path set, so it can never dangle. (An
    // `IntendedFor` still can, by design — hence the restriction to schema-derived kinds.)
    let dangling_structural: i64 = db.conn.query_row(
        "SELECT COUNT(*) FROM file_associations \
         WHERE association_type IN ('bval', 'bvec', 'events', 'channels') \
           AND target_file_id IS NULL",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(dangling_structural, 0);

    // ds000117 ships a sibling gradient pair per acquisition, so the edges are 1:1 there —
    // the counterpart to ds114's 20:1, and the reason `curated.rs`'s row counts are unchanged.
    let (bvals, targets): (i64, i64) = db.conn.query_row(
        "SELECT COUNT(*), COUNT(DISTINCT target_file_path) FROM file_associations \
         WHERE association_type = 'bval'",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    assert_eq!(bvals, 11);
    assert_eq!(targets, 11, "one gradient file per image, not one shared");
    Ok(())
}
