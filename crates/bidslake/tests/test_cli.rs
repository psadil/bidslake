//! End-to-end CLI checks for the overlay experimentation surfaces (`schema --diff`)
//! and dataset-embedded overlay auto-discovery.

use std::fs;
use std::process::Command;

/// `bidslake schema --adapter fmriprep --diff` builds from the embedded schema +
/// bundled overlay (no dataset, no database) and reports the new `fmriprep_confounds`
/// table the overlay adds.
#[test]
fn schema_diff_reports_overlay_additions() {
    let output = Command::new(env!("CARGO_BIN_EXE_bidslake"))
        .args(["schema", "--adapter", "fmriprep", "--diff"])
        .output()
        .expect("run bidslake");
    assert!(
        output.status.success(),
        "schema --diff failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("new table fmriprep_confounds"),
        "diff should announce the new confounds table:\n{stdout}"
    );
    assert!(
        stdout.contains("trans_x"),
        "diff should list the typed confound columns:\n{stdout}"
    );
}

/// An adapter is a *named bundle*, and its three artifacts are each optional: a name
/// resolves if bidslake ships any of an overlay, a term map, or an ingestion fragment
/// under it. `freesurfer` has all three; `fmriprep` has an overlay and an ingestion
/// fragment but no term map (its filenames already carry BIDS entities), which is the
/// case that used to be rejected outright.
#[test]
fn adapter_resolves_with_a_partial_artifact_set() {
    for name in ["fmriprep", "mriqc", "qsiprep", "freesurfer"] {
        let output = Command::new(env!("CARGO_BIN_EXE_bidslake"))
            .args(["schema", "--adapter", name, "--diff"])
            .output()
            .expect("run bidslake");
        assert!(
            output.status.success(),
            "--adapter {name} should resolve: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

/// An unknown adapter names the union of the bundled registries, not just one of them.
#[test]
fn unknown_adapter_lists_every_bundled_name() {
    let output = Command::new(env!("CARGO_BIN_EXE_bidslake"))
        .args(["schema", "--adapter", "nosuchpipeline", "--diff"])
        .output()
        .expect("run bidslake");
    assert!(!output.status.success(), "unknown adapter should fail");
    let stderr = String::from_utf8_lossy(&output.stderr);
    for name in ["fmriprep", "freesurfer", "mriqc", "qsiprep"] {
        assert!(stderr.contains(name), "should list {name}:\n{stderr}");
    }
}

/// `--overlay` takes paths only. A bundled name there would apply the vocabulary but
/// not the rest of the bundle, so it is rejected with a message naming the fix.
#[test]
fn overlay_rejects_a_bundled_name() {
    let output = Command::new(env!("CARGO_BIN_EXE_bidslake"))
        .args(["schema", "--overlay", "fmriprep", "--diff"])
        .output()
        .expect("run bidslake");
    assert!(!output.status.success(), "a bundled name is not a path");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--adapter fmriprep"),
        "error should point at the flag that works:\n{stderr}"
    );
}

/// With no overlays, `schema --diff` reports no changes.
#[test]
fn schema_diff_empty_without_overlays() {
    let output = Command::new(env!("CARGO_BIN_EXE_bidslake"))
        .args(["schema", "--diff"])
        .output()
        .expect("run bidslake");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("No schema changes"), "got:\n{stdout}");
}

/// A `.bidslake/overlay.json` at the dataset root is auto-applied with no `--overlay`
/// flag: `index --dry-run` then ingests (not skips) the confounds timeseries.
#[test]
fn embedded_overlay_is_auto_discovered() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("sub-01/func")).unwrap();
    fs::create_dir_all(root.join(".bidslake")).unwrap();
    fs::write(
        root.join("dataset_description.json"),
        r#"{"Name":"deriv","BIDSVersion":"1.11.1","DatasetType":"derivative"}"#,
    )
    .unwrap();
    fs::write(
        root.join("sub-01/func/sub-01_task-rest_desc-confounds_timeseries.tsv"),
        "trans_x\ttrans_y\n0.1\t0.2\n0.3\t0.4\n",
    )
    .unwrap();
    // The embedded overlay is the bundled fMRIPrep one (copied to the dataset).
    let fmriprep = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../bids-schema/data/overlays/fmriprep.json"
    );
    fs::copy(fmriprep, root.join(".bidslake/overlay.json")).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_bidslake"))
        .args(["index", "-i"])
        .arg(root)
        .args(["-o", "unused.duckdb", "--dry-run"])
        .output()
        .expect("run bidslake");
    assert!(
        output.status.success(),
        "index --dry-run failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("dataset-embedded overlay"),
        "should announce discovery:\n{stdout}"
    );
    assert!(
        stdout.contains("ingested: 1") && stdout.contains("No skipped tabular files"),
        "embedded overlay should make the confounds ingested, not skipped:\n{stdout}"
    );
}

/// A pipeline's `.bidsignore` hides its non-standard outputs (fMRIPrep lists
/// `*_timeseries.tsv`); `--no-bidsignore` walks them anyway so an overlay can index
/// them. Without the flag the file is invisible; with it, the overlay ingests it.
#[test]
fn no_bidsignore_reveals_hidden_overlay_files() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("sub-01/func")).unwrap();
    fs::write(
        root.join("dataset_description.json"),
        r#"{"Name":"deriv","BIDSVersion":"1.11.1","DatasetType":"derivative"}"#,
    )
    .unwrap();
    fs::write(root.join(".bidsignore"), "*_timeseries.tsv\n").unwrap();
    fs::write(
        root.join("sub-01/func/sub-01_task-rest_desc-confounds_timeseries.tsv"),
        "trans_x\ttrans_y\n0.1\t0.2\n",
    )
    .unwrap();

    let run = |extra: &[&str]| {
        let mut args = vec![
            "index",
            "-i",
            root.to_str().unwrap(),
            "-o",
            "x.duckdb",
            "--dry-run",
        ];
        args.extend_from_slice(extra);
        let out = Command::new(env!("CARGO_BIN_EXE_bidslake"))
            .args(&args)
            .output()
            .expect("run bidslake");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    };

    // Default: the confounds file is hidden by .bidsignore — nothing tabular seen.
    let hidden = run(&["--adapter", "fmriprep"]);
    assert!(
        hidden.contains("No skipped tabular files") && !hidden.contains("ingested:"),
        "with .bidsignore in effect, the confounds file should not be walked:\n{hidden}"
    );

    // --no-bidsignore reveals it, and the overlay ingests it.
    let revealed = run(&["--adapter", "fmriprep", "--no-bidsignore"]);
    assert!(
        revealed.contains("ingested: 1"),
        "--no-bidsignore should let the overlay ingest the hidden confounds:\n{revealed}"
    );
}
