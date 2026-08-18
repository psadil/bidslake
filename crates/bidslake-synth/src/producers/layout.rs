//! The general producer: every role of a layout, under every one of its examples.
//!
//! There is no recipe here, and that is the point of a layout. The document already declares
//! each path in the tree, and its `Examples` already carry a unit root the term map can parse
//! plus a binding for every placeholder the roles use — both vouched for at load time by
//! `validate_round_trip`. So generating the tree is: rewrite the example root's subject, render
//! every role under it, and ask the ingestion schema what each result would become.
//!
//! This is what makes the crate an authoring tool rather than only a scale generator. Point it
//! at a layout under development with `--layout path.json` and it materializes the tree that
//! layout describes, which is the fastest way to find out that a hand-written PCRE claims one
//! path and not its sibling.
//!
//! ## Why only `--subjects` scales a layout tree
//!
//! A layout's unit granularity is the layout's own business, declared by its examples: `feat`'s
//! root is one BOLD run (`sub-01_ses-V1_task-rest_run-01_desc-preproc_bold`), `freesurfer`'s is
//! one subject-session (`sub-01_ses-V1`). Multiplying that by `--sessions` and `--runs` would
//! mean rewriting entities an example may not carry, and two examples that differ in which
//! entities they carry would then scale at different rates — so the file count would stop being
//! linear in anything, which is the one property the benchmark thesis needs.
//!
//! Instead `--subjects N` means N *rounds*, each contributing one unit per example, each unit
//! under its own subject label. So a layout with three examples yields `3N` subject directories,
//! the file count stays exactly linear in N, and at `--subjects 1` the tree is the examples
//! themselves — which is the authoring case.

use std::collections::BTreeMap;

use anyhow::{Result, anyhow};
use bids_schema::layout::Layout;
use bids_schema::term_map::{TermMap, bundled_term_map};
use bidslake::schema::Schema;
use bidslake::schema::ingestion::Disposition;
use bidslake::schema::tabular::FileContext;
use serde_json::Value;

use crate::{Claim, PlannedFile, Scale, subject_label, tabular};

/// Every file the tree this layout describes holds, at `scale.subjects` copies per example.
pub fn plan(schema: &Schema, layout: &Layout, scale: &Scale) -> Result<Vec<PlannedFile>> {
    let term_map = bundled_term_map(layout.term_map_name()).ok_or_else(|| {
        anyhow!(
            "layout names term map {:?}, which is not bundled",
            layout.term_map_name()
        )
    })?;

    let mut files = Vec::new();
    let examples = layout.examples();
    for round in 0..scale.subjects.max(1) {
        for (index, example) in examples.iter().enumerate() {
            // A label per (round, example), not per round. Two examples can differ only in a way
            // the rewrite erases — `freesurfer`'s `sub-02` and its bare `bert` both become one
            // subject — and then they render the same paths, so the tree is written twice and
            // the third subject-dir form the examples exist to cover disappears.
            let subject = subject_label(round * examples.len() + index);
            let root = rewrite_subject(&example.root, &subject);
            for role in layout.roles() {
                // `None` is an unbound placeholder, and treating it as a skip is how a tree
                // silently loses files as a layout grows roles. The examples are supposed to
                // bind everything — `validate_round_trip` says so — so this is a real error.
                let rendered = layout.render(role, &example.bindings).ok_or_else(|| {
                    anyhow!(
                        "layout role {role:?} does not render under example {:?}; its \
                         placeholders are unbound, which `Examples` are supposed to prevent",
                        example.root
                    )
                })?;
                let rel_path = format!("{root}/{rendered}");
                files.push(materialize(
                    schema,
                    &term_map,
                    layout.term_map_name(),
                    rel_path,
                    scale,
                ));
            }
        }
    }
    Ok(files)
}

/// One rendered role: ask the term map what it is, then the ingestion schema what to do with it,
/// then write a body the named reader can actually parse.
fn materialize(
    schema: &Schema,
    term_map: &TermMap,
    term_map_name: &str,
    rel_path: String,
    scale: &Scale,
) -> PlannedFile {
    let claim = Claim::Projected {
        term_map: term_map_name.to_string(),
    };
    let Some(facts) = term_map.classify(&rel_path) else {
        // Unreachable for a bundled layout, whose round trip proves every role classifies, but
        // reachable for a layout under development. Claiming it as projected anyway is what
        // makes `verify` report it, which is the answer that author needs.
        return PlannedFile::empty(rel_path, claim);
    };

    let slashed = format!("/{rel_path}");
    let null = Value::Null;
    let ctx = FileContext {
        path: &slashed,
        datatype: facts.datatype.as_deref(),
        suffix: facts.suffix.as_deref(),
        extension: facts.extension.as_deref(),
        sidecar: &null,
        dataset_type: Some("derivative"),
    };

    let rule = schema.ingestion().classify(&ctx);
    let reader = match rule {
        Some(r) if r.disposition == Disposition::Read => r.reader.as_deref(),
        _ => None,
    };
    let Some(reader) = reader else {
        return PlannedFile::empty(rel_path, claim);
    };

    let Some(spec) = schema.tabular().route(&ctx) else {
        // A `read` disposition whose file routes to no table: the reader would be handed no
        // columns. Empty rather than invented, so the situation stays visible under `--explain`.
        return PlannedFile::empty(rel_path, claim);
    };

    let body = match reader {
        "fs_stats" => tabular::fs_stats(schema, spec, scale.confound_rows),
        "matrix" => tabular::matrix(schema, spec, scale.confound_rows),
        "csv" => tabular::tsv(schema, spec, scale.confound_rows, &BTreeMap::new()),
        _ => return PlannedFile::empty(rel_path, claim),
    };
    PlannedFile::text(rel_path, claim, body)
}

/// Rewrite the subject label in a unit root, leaving every other segment alone.
///
/// Only the subject, and only its value: a root has to stay parseable by the term map that
/// vouched for it, and every other segment — `ses-V1`, `desc-preproc_bold` — is part of what
/// makes it so.
///
/// **The `sub-` prefix is preserved as the example wrote it.** FreeSurfer's term map admits three
/// subject-dir forms and the bundled layout has an example for each; a rewrite that added the
/// prefix everywhere would turn the bare `bert` into `sub-0003` and stop exercising the form that
/// example exists for. So `sub-01_ses-V1` becomes `sub-0001_ses-V1` and `bert` becomes `0003`.
fn rewrite_subject(root: &str, subject: &str) -> String {
    let mut segments: Vec<String> = root.split('_').map(str::to_string).collect();
    match segments.first() {
        Some(first) if first.starts_with("sub-") => segments[0] = format!("sub-{subject}"),
        Some(_) => segments[0] = subject.to_string(),
        None => return subject.to_string(),
    }
    segments.join("_")
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case::session_form("sub-01_ses-V1", "sub-0007_ses-V1")]
    #[case::sessionless("sub-02", "sub-0007")]
    // Stays bare. Prefixing it would collapse FreeSurfer's third subject-dir form onto its
    // second, and that form is the only reason its example exists.
    #[case::bare_freesurfer_label("bert", "0007")]
    #[case::feat_unit(
        "sub-01_ses-V1_task-rest_run-01_desc-preproc_bold",
        "sub-0007_ses-V1_task-rest_run-01_desc-preproc_bold"
    )]
    fn only_the_subject_segment_is_rewritten(#[case] root: &str, #[case] expected: &str) {
        let rewritten = rewrite_subject(root, "0007");

        assert_eq!(rewritten, expected);
    }

    /// Two examples must never render the same unit root, or the tree is written twice and one
    /// example stops covering anything. `freesurfer` is the case: `sub-02` and `bert` differ only
    /// in the prefix that a naive rewrite would add to both.
    #[rstest]
    #[case::freesurfer("freesurfer")]
    #[case::feat("feat")]
    fn every_example_gets_its_own_unit_root(#[case] name: &str) {
        let layout = bids_schema::layout::bundled_layout(name).expect("bundled layout");
        let examples = layout.examples();

        let roots: std::collections::BTreeSet<String> = examples
            .iter()
            .enumerate()
            .map(|(i, e)| rewrite_subject(&e.root, &crate::subject_label(i)))
            .collect();

        assert_eq!(roots.len(), examples.len(), "{name}: {roots:?}");
    }

    /// The rewritten root has to stay something the term map can parse, or every role under it
    /// silently stops classifying — which looks exactly like a broken term map.
    #[rstest]
    #[case::freesurfer("freesurfer", "stats/aseg.stats")]
    #[case::feat("feat", "mask.nii.gz")]
    fn a_rewritten_root_still_classifies(#[case] name: &str, #[case] role_path: &str) {
        let term_map = bundled_term_map(name).expect("bundled term map");
        let layout = bids_schema::layout::bundled_layout(name).expect("bundled layout");
        let root = rewrite_subject(&layout.examples()[0].root, "0007");

        let facts = term_map.classify(&format!("{root}/{role_path}"));

        assert!(facts.is_some(), "{root}/{role_path} classified as nothing");
    }
}
