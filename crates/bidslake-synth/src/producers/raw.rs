//! Raw BIDS — the producer that really is enumerated from the schema.
//!
//! Filenames come from [`bids_schema::naming`], so the entity order is `rules.entities`' and not
//! this file's. Table bodies come from whatever `rules.tabular_data` routes each path to, so
//! `participants.tsv`, `sessions.tsv` and `scans.tsv` carry the columns the standard declares —
//! and those three are exactly what no dataset in the ingest benchmark ships, which is the gap
//! that made a regression in the batched tabular path invisible.
//!
//! Sidecar keys come from `rules.sidecars`, which is what makes the acceptance bar reachable: a
//! generated raw tree has to pass `bids-validator-rs` with zero errors, and the fields that
//! validator demands are the fields those rules name.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{Result, anyhow};
use bids_schema::naming::{BidsName, NameIndex};
use bidslake::schema::Schema;
use serde_json::json;

use crate::producers::{overrides, raw_description, readme};
use crate::{
    Claim, PlannedFile, Scale, nifti, run_label, session_label, sidecar, subject_label, tabular,
};

/// The `RepetitionTime` every generated run declares, and the fourth `pixdim` its header carries.
/// The two have to agree or the validator has a check for it.
const REPETITION_TIME: f64 = 2.0;

/// Every file a raw tree of this scale holds.
pub fn plan(schema: &Schema, scale: &Scale) -> Result<Vec<PlannedFile>> {
    let index = NameIndex::new(schema.raw());
    let mut files = Vec::new();

    files.push(PlannedFile::text(
        "dataset_description.json",
        // Matched by a `path ==` rule in `rules.files.common.core`, not by a suffix, so there is
        // no suffix claim to make about it.
        Claim::Unclaimed,
        raw_description("synthetic-raw"),
    ));
    files.push(PlannedFile::text(
        "README",
        Claim::Unclaimed,
        readme("synthetic-raw"),
    ));

    let subjects: Vec<String> = (0..scale.subjects).map(subject_label).collect();
    files.push(participants(schema, &subjects)?);

    for subject in &subjects {
        let sessions: Vec<String> = (0..scale.sessions.max(1)).map(session_label).collect();
        files.push(sessions_table(schema, subject, &sessions)?);

        for session in &sessions {
            let mut session_files = Vec::new();

            let anat = BidsName::new("T1w", ".nii.gz")
                .entity("sub", subject)
                .entity("ses", session)
                .datatype("anat");
            session_files.push(emit(schema, &index, anat, scale)?);

            for (run_index, run) in (0..scale.runs.max(1)).map(run_label).enumerate() {
                let task = &scale.tasks[run_index % scale.tasks.len()];
                let bold = BidsName::new("bold", ".nii.gz")
                    .entity("sub", subject)
                    .entity("ses", session)
                    .entity("task", task)
                    .entity("run", &run)
                    .datatype("func");
                session_files.push(emit(schema, &index, bold, scale)?);

                let events = BidsName::new("events", ".tsv")
                    .entity("sub", subject)
                    .entity("ses", session)
                    .entity("task", task)
                    .entity("run", &run)
                    .datatype("func");
                session_files.push(emit(schema, &index, events, scale)?);
            }

            // The paths in a `scans.tsv` are resolved relative to **the scans file's own
            // directory** — `exists(columns.filename, "file")` in the schema's check, and
            // `bids_schema::expression`'s `"file"` rule joins onto the containing directory. For
            // a session-level scans file that is `sub-X/ses-Y/`, so the rows read `anat/…`, not
            // `ses-Y/anat/…`. Getting it wrong is invisible until something resolves one, which
            // is what `SCANS_FILENAME_NOT_MATCH_DATASET` is.
            let prefix = format!("sub-{subject}/ses-{session}/");
            let described: Vec<String> = session_files
                .iter()
                .flat_map(|group| group.iter())
                .filter(|f| f.rel_path.ends_with(".nii.gz") || f.rel_path.ends_with("_events.tsv"))
                .map(|f| f.rel_path.trim_start_matches(&prefix).to_string())
                .collect();
            files.push(scans_table(schema, subject, session, &described)?);
            files.extend(session_files.into_iter().flatten());
        }
    }
    Ok(files)
}

/// One data file, plus the JSON sidecar its `rules.sidecars` entry demands and the body a
/// `.tsv` needs.
fn emit(
    schema: &Schema,
    index: &NameIndex,
    name: BidsName,
    scale: &Scale,
) -> Result<Vec<PlannedFile>> {
    let path = name.render_path(index)?;
    let filename = path.rsplit('/').next().unwrap_or(&path).to_string();
    let parts = bids_core::entities::read_entities(&filename);
    let datatype = path.split('/').nth_back(1).map(str::to_string);
    let entity_map: BTreeMap<String, String> = parts
        .entities
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    let mut out = Vec::new();

    if parts.extension == ".tsv" {
        let spec = tabular::route(
            schema,
            &path,
            datatype.as_deref(),
            Some(&parts.suffix),
            Some(&parts.extension),
            Some("raw"),
        )
        .ok_or_else(|| anyhow!("no tabular rule routes {path}"))?;
        let body = tabular::tsv(schema, spec, scale.confound_rows, &BTreeMap::new());
        out.push(PlannedFile::text(path, Claim::Bids, body));
        return Ok(out);
    }

    // A run is 4-D and its fourth `pixdim` is the TR the sidecar declares; an anat is 3-D.
    let volumes = (parts.suffix == "bold").then_some(scale.confound_rows.max(1));
    out.push(PlannedFile::bytes(
        path.clone(),
        Claim::Bids,
        Arc::from(nifti::gzipped_volume(volumes, REPETITION_TIME)),
    ));

    let ctx = sidecar::SidecarContext {
        path: &format!("/{path}"),
        datatype: datatype.as_deref(),
        suffix: Some(&parts.suffix),
        extension: Some(&parts.extension),
        entities: &entity_map,
        dataset_type: Some("raw"),
    };
    let task = entity_map.get("task").cloned().unwrap_or_default();
    let mut declared = overrides([("EchoTime", json!(0.03))]);
    if parts.suffix == "bold" {
        declared.insert("TaskName".to_string(), json!(task));
        declared.insert("RepetitionTime".to_string(), json!(REPETITION_TIME));
    }
    let body = sidecar::body(schema.raw(), &ctx, &declared);
    let sidecar_path = format!("{}.json", path.trim_end_matches(&parts.extension));
    out.push(PlannedFile::text(sidecar_path, Claim::Bids, body));
    Ok(out)
}

fn participants(schema: &Schema, subjects: &[String]) -> Result<PlannedFile> {
    let spec = tabular::route(
        schema,
        "participants.tsv",
        None,
        Some("participants"),
        Some(".tsv"),
        Some("raw"),
    )
    .ok_or_else(|| anyhow!("no tabular rule routes participants.tsv"))?;
    let ids: Vec<String> = subjects.iter().map(|s| format!("sub-{s}")).collect();
    let body = tabular::tsv(
        schema,
        spec,
        subjects.len(),
        &BTreeMap::from([("participant_id".to_string(), ids)]),
    );
    Ok(PlannedFile::text(
        "participants.tsv",
        Claim::Unclaimed,
        body,
    ))
}

fn sessions_table(schema: &Schema, subject: &str, sessions: &[String]) -> Result<PlannedFile> {
    let path = format!("sub-{subject}/sub-{subject}_sessions.tsv");
    let spec = tabular::route(
        schema,
        &path,
        None,
        Some("sessions"),
        Some(".tsv"),
        Some("raw"),
    )
    .ok_or_else(|| anyhow!("no tabular rule routes {path}"))?;
    let ids: Vec<String> = sessions.iter().map(|s| format!("ses-{s}")).collect();
    let body = tabular::tsv(
        schema,
        spec,
        sessions.len(),
        &BTreeMap::from([("session_id".to_string(), ids)]),
    );
    Ok(PlannedFile::text(path, Claim::Bids, body))
}

fn scans_table(
    schema: &Schema,
    subject: &str,
    session: &str,
    described: &[String],
) -> Result<PlannedFile> {
    let path = format!("sub-{subject}/ses-{session}/sub-{subject}_ses-{session}_scans.tsv");
    let spec = tabular::route(
        schema,
        &path,
        None,
        Some("scans"),
        Some(".tsv"),
        Some("raw"),
    )
    .ok_or_else(|| anyhow!("no tabular rule routes {path}"))?;
    let body = tabular::tsv(
        schema,
        spec,
        described.len(),
        &BTreeMap::from([("filename".to_string(), described.to_vec())]),
    );
    Ok(PlannedFile::text(path, Claim::Bids, body))
}
