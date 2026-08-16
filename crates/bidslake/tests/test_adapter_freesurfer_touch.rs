//! recon-all's `touch/` stamps are ignored, and the stats files still read.
//!
//! `touch/` holds one empty file per recon-all stage, recording that it ran. They carry no
//! data — 69 of them for a single subject — and their names collide with the stats rules:
//! `touch/segstats.touch` classified as `suffix == "segstats"` and was routed to the
//! `fs_stats` reader, which warned on every index and read nothing.
//!
//! The rule matches the *directory* rather than the `.touch` extension. That is the
//! stronger statement — `touch/` is where recon-all puts its stamps, whatever it calls
//! them, and it catches the variants (`lh.aparcstats2.touch`) that an extension match on a
//! doubly-dotted name would have to reason about.

use bidslake::schema::ingestion::{Disposition, Ingestion};
use bidslake::schema::tabular::FileContext;
use rstest::rstest;

fn freesurfer() -> Ingestion {
    let srcs = [
        bids_schema::bundled_ingestion_source("base").expect("base"),
        bids_schema::bundled_ingestion_source("freesurfer").expect("freesurfer"),
    ];
    Ingestion::from_sources(&srcs).expect("ingestion fragments load")
}

fn classify(
    ing: &Ingestion,
    path: &str,
    suffix: Option<&str>,
) -> Option<(Disposition, Option<String>)> {
    let null = serde_json::Value::Null;
    let ctx = FileContext {
        path,
        datatype: None,
        suffix,
        extension: None,
        sidecar: &null,
        dataset_type: None,
    };
    ing.classify(&ctx)
        .map(|r| (r.disposition, r.reader.clone()))
}

// The one that produced the warning.
#[rstest]
#[case::segstats("/sub-01_ses-V1/touch/segstats.touch", Some("segstats"))]
// The `parcstats` variant, and a hemisphere-prefixed name.
#[case::hemisphere_parcstats("/sub-01_ses-V1/touch/lh.aparcstats2.touch", Some("parcstats"))]
// A stamp whose name matches no stats rule at all — still ignored, because the rule is
// about the directory rather than about which reader would have claimed it.
#[case::no_matching_stats_rule("/sub-01_ses-V1/touch/conform.touch", None)]
fn touch_stamps_are_ignored(#[case] path: &str, #[case] suffix: Option<&str>) {
    let ing = freesurfer();

    let got = classify(&ing, path, suffix).map(|(d, _)| d);

    assert_eq!(got, Some(Disposition::Ignore), "{path} should be ignored");
}

#[test]
fn the_stats_files_they_shadowed_still_read() {
    // The other half: ignoring `touch/` must not cost the real stats files, which are what
    // fill `freesurfer_aseg`/`_aparc`/`_measures`.
    // The `segstats` route is pinned in-crate by
    // `bidslake::schema::ingestion::tests::stats_files_are_read_by_fs_stats`, over the same
    // fragment and so past the same `touch/` rule.
    let ing = freesurfer();
    assert_eq!(
        classify(
            &ing,
            "/sub-01_ses-V1/stats/lh.aparc.stats",
            Some("parcstats")
        ),
        Some((Disposition::Read, Some("fs_stats".to_string())))
    );
    assert_eq!(
        classify(&ing, "/sub-01_ses-V1/label/aseg.ctab", Some("fslabels")),
        Some((Disposition::Read, Some("fs_ctab".to_string())))
    );
}
