//! An fMRIPrep-shaped derivative tree.
//!
//! The one producer whose paths are a recipe rather than an enumeration, and it is worth saying
//! exactly why rather than leaving it to look like laziness. `timeseries`, `xfm` and `boldref`
//! are declared by the `fmriprep` overlay as *vocabulary* — `objects.suffixes` — and by nothing
//! as *files*: no overlay in the repo touches `rules.files`, and base `rules.files.deriv` names
//! none of the three. There is no rule group to walk, so the shapes below are written out, and
//! `every_overlay_suffix_is_emitted_by_some_producer` is what stops them falling behind the
//! vocabulary.
//!
//! The surface family below is the exception to that paragraph, and the reason it is worth
//! reading twice. It used to be absent: fMRIPrep names its morphometry maps by measure —
//! `_hemi-L_thickness.shape.gii` — and none of `thickness`/`curv`/`sulc` nor `.shape.gii` was
//! declared anywhere, so nothing written here would have indexed. The overlay briefly carried a
//! `morph` suffix covering the family, which no tool writes; emitting a `_hemi-L_morph.shape.gii`
//! to satisfy the coverage test was vocabulary-faithful and tool-unfaithful, and the suffix was
//! dropped instead.
//!
//! The always-applied `bep011` overlay settled it by mirroring what BIDS itself is adopting, and
//! it declares `rules.files.deriv.structural_mri` as well as the vocabulary. So these paths are
//! the one part of this producer a rule group *does* describe, and the guarantee is stronger
//! than the suffix census: `a_generated_fmriprep_tree_has_no_validator_errors` runs the real
//! validator over the tree, which is only silent if the names, their entities and their
//! extensions all match what the schema declares.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{Result, anyhow};
use bids_schema::naming::{BidsName, NameIndex};
use bidslake::schema::Schema;
use serde_json::json;

use crate::producers::{derivative_description, overrides};
use crate::{
    Claim, PlannedFile, Scale, nifti, run_label, session_label, sidecar, subject_label, tabular,
};

/// The TR every generated run declares, and the fourth `pixdim` its header carries. fMRIPrep's
/// own trees are fast-TR multiband; 0.8 s is what the a2cps tree this is modelled on used.
const REPETITION_TIME: f64 = 0.8;

/// The patterns fMRIPrep really writes into `.bidsignore` — and note that three of them hide the
/// files this tree exists to exercise, which is why writing it at all is opt-in. `*.surf.gii` is
/// among them and `*.shape.gii` is not, so a real fMRIPrep tree hides its surface *geometry* from
/// an ordinary ingest while leaving the morphometry maps visible.
const BIDSIGNORE: &str =
    "*.html\nlogs/\nfigures/\n*_xfm.*\n*_timeseries.tsv\n*.surf.gii\n*_bold.func.gii\n";

/// Every file an fMRIPrep tree of this scale holds.
pub fn plan(schema: &Schema, scale: &Scale, bidsignore: bool) -> Result<Vec<PlannedFile>> {
    let index = NameIndex::new(schema.raw());
    let mut files = vec![PlannedFile::text(
        "dataset_description.json",
        Claim::Unclaimed,
        derivative_description("synthetic-fmriprep", "fMRIPrep", "25.2.5"),
    )];
    if bidsignore {
        files.push(PlannedFile::text(
            ".bidsignore",
            Claim::Unclaimed,
            BIDSIGNORE,
        ));
    }

    // Built once and shared by every run: at the a2cps shape this body is ~2 MB, and a thousand
    // runs holding their own copy would be gigabytes of nothing.
    let confounds_body = confounds_body(schema, scale)?;
    let confounds_sidecar: Arc<[u8]> = Arc::from(confounds_sidecar(scale).into_bytes());
    // One volume for every imaging file in the tree: the bytes are identical, so a
    // hundred-thousand-file tree holds one copy rather than a hundred thousand.
    let anat_volume: Arc<[u8]> = Arc::from(nifti::gzipped_volume(None, REPETITION_TIME));
    let bold_volume: Arc<[u8]> = Arc::from(nifti::gzipped_volume(
        Some(scale.confound_rows.max(1)),
        REPETITION_TIME,
    ));

    for subject in (0..scale.subjects).map(subject_label) {
        for session in (0..scale.sessions.max(1)).map(session_label) {
            // `anat` lives under `ses-` even though BIDS permits it at subject level: a
            // session-scoped join on `(sub, ses)` finds nothing for a row whose `ses` is NULL,
            // and every downstream layout that consumes an fMRIPrep tree joins that way.
            let space = &scale.spaces[0];
            for name in [
                BidsName::new("T1w", ".nii.gz").entity("desc", "preproc"),
                BidsName::new("dseg", ".nii.gz"),
                BidsName::new("xfm", ".h5")
                    .entity("from", "T1w")
                    .entity("to", space)
                    .entity("mode", "image"),
                BidsName::new("xfm", ".h5")
                    .entity("from", space)
                    .entity("to", "T1w")
                    .entity("mode", "image"),
            ] {
                let path = name
                    .entity("sub", &subject)
                    .entity("ses", &session)
                    .datatype("anat")
                    .render_path(&index)?;
                // Only the NIfTIs get a volume. An `.h5` transform is opaque to every reader in
                // the workspace, so bytes there would be parsed by nothing and validated by
                // nothing — the two reasons a volume is worth writing at all.
                if path.ends_with(".nii.gz") {
                    files.push(PlannedFile::bytes(path, Claim::Bids, anat_volume.clone()));
                } else {
                    files.push(PlannedFile::empty(path, Claim::Bids));
                }
            }

            // Surface morphometry and geometry. Named by measure, the way fMRIPrep writes them,
            // and the whole point of the shared vocabulary: `_hemi-L_thickness.shape.gii` here
            // and FreeSurfer's positional `surf/lh.thickness` are one quantity under one suffix,
            // reachable by one predicate.
            //
            // No volume is written, for the reason the `.h5` transforms above get none: GIFTI is
            // opaque to every reader in the workspace, so bytes here would be parsed by nothing.
            for hemi in ["L", "R"] {
                for (suffix, extension) in [
                    ("thickness", ".shape.gii"),
                    ("curv", ".shape.gii"),
                    ("sulc", ".shape.gii"),
                    ("white", ".surf.gii"),
                    ("pial", ".surf.gii"),
                    ("midthickness", ".surf.gii"),
                    ("inflated", ".surf.gii"),
                ] {
                    let path = BidsName::new(suffix, extension)
                        .entity("sub", &subject)
                        .entity("ses", &session)
                        .entity("hemi", hemi)
                        .datatype("anat")
                        .render_path(&index)?;
                    files.push(PlannedFile::empty(path, Claim::Bids));
                }
            }

            for (run_index, run) in (0..scale.runs.max(1)).map(run_label).enumerate() {
                let task = &scale.tasks[run_index % scale.tasks.len()];
                let func = |name: BidsName| -> BidsName {
                    name.entity("sub", &subject)
                        .entity("ses", &session)
                        .entity("task", task)
                        .entity("run", &run)
                        .datatype("func")
                };

                let preproc = func(BidsName::new("bold", ".nii.gz").entity("desc", "preproc"));
                let preproc_path = preproc.render_path(&index)?;
                files.push(PlannedFile::bytes(
                    preproc_path.clone(),
                    Claim::Bids,
                    bold_volume.clone(),
                ));
                files.push(bold_sidecar(schema, &preproc_path, task)?);

                for name in [
                    BidsName::new("mask", ".nii.gz").entity("desc", "brain"),
                    BidsName::new("boldref", ".nii.gz"),
                ] {
                    files.push(PlannedFile::bytes(
                        func(name).render_path(&index)?,
                        Claim::Bids,
                        anat_volume.clone(),
                    ));
                }

                // Each `space-` resampling is another data file the one confounds table
                // describes, which is the many-to-many an association has to fan out — a 1:1
                // tree cannot tell a re-key from a copy.
                for space in &scale.spaces {
                    let resampled = func(
                        BidsName::new("bold", ".nii.gz")
                            .entity("space", space)
                            .entity("desc", "preproc"),
                    );
                    files.push(PlannedFile::bytes(
                        resampled.render_path(&index)?,
                        Claim::Bids,
                        bold_volume.clone(),
                    ));
                }

                let confounds = func(
                    BidsName::new("timeseries", ".tsv")
                        .entity("desc", "confounds")
                        .clone(),
                );
                let confounds_path = confounds.render_path(&index)?;
                files.push(PlannedFile::bytes(
                    confounds_path.clone(),
                    Claim::Bids,
                    confounds_body.clone(),
                ));
                files.push(PlannedFile::bytes(
                    format!("{}.json", confounds_path.trim_end_matches(".tsv")),
                    Claim::Bids,
                    confounds_sidecar.clone(),
                ));
            }
        }
    }
    Ok(files)
}

fn confounds_body(schema: &Schema, scale: &Scale) -> Result<Arc<[u8]>> {
    let probe =
        "sub-0001/ses-V1/func/sub-0001_ses-V1_task-rest_run-01_desc-confounds_timeseries.tsv";
    let spec = tabular::route(
        schema,
        probe,
        Some("func"),
        Some("timeseries"),
        Some(".tsv"),
        Some("derivative"),
    )
    .ok_or_else(|| {
        anyhow!(
            "no tabular rule routes a confounds file; the `fmriprep` overlay is what declares \
             one, so this schema was loaded without it"
        )
    })?;
    let declared = spec.columns.len();
    let extra = scale.confound_columns.saturating_sub(declared);
    Ok(Arc::from(
        tabular::confounds(schema, spec, scale.confound_rows, extra).into_bytes(),
    ))
}

/// The column dictionary fMRIPrep ships beside a confounds table: one object per column, read in
/// full by the ingest, and on a real tree larger than most sidecars in the dataset.
fn confounds_sidecar(scale: &Scale) -> String {
    let entries: serde_json::Map<String, serde_json::Value> = (0..scale.confound_columns)
        .map(|i| {
            (
                format!("a_comp_cor_{i:04}"),
                json!({ "Method": "aCompCor", "Retained": true }),
            )
        })
        .collect();
    serde_json::to_string(&serde_json::Value::Object(entries)).expect("a JSON object serializes")
}

fn bold_sidecar(schema: &Schema, data_path: &str, task: &str) -> Result<PlannedFile> {
    let entities = BTreeMap::from([("task".to_string(), task.to_string())]);
    let ctx = sidecar::SidecarContext {
        path: &format!("/{data_path}"),
        datatype: Some("func"),
        suffix: Some("bold"),
        extension: Some(".nii.gz"),
        entities: &entities,
        dataset_type: Some("derivative"),
    };
    let body = sidecar::body(
        schema.raw(),
        &ctx,
        &overrides([
            ("TaskName", json!(task)),
            ("RepetitionTime", json!(REPETITION_TIME)),
            ("SkullStripped", json!(false)),
        ]),
    );
    Ok(PlannedFile::text(
        format!("{}.json", data_path.trim_end_matches(".nii.gz")),
        Claim::Bids,
        body,
    ))
}
