#!/usr/bin/env python3
"""Generate a synthetic BIDS tree at a chosen scale, for performance work.

`bids-examples` is the correctness corpus, not a performance one: its largest
dataset is ~2,400 files, and the costs that matter at 100k+ are the ones that are
invisible there. Two in particular scale with *shape* rather than size, so the
parameters below are deliberately independent — sweep one at a time and a
superlinear term shows up as a bend in the curve:

  --subjects   widens the dataset root, which is what a per-directory linear scan
               is quadratic in.
  --runs       deepens the per-suffix sidecar bucket, which is what sidecar
               matching is quadratic in.

Only files something actually reads need contents; imaging files are created
empty, so a 100k-file tree costs seconds to write and little disk.

Two shapes, because they stress different paths:

  raw       one `_bold.json` per run — the layout where the number of sidecars
            sharing a suffix grows with the dataset.
  fmriprep  wide `desc-confounds_timeseries.tsv` plus its large column
            dictionary — the layout where per-file body size and the batched
            tabular ingest dominate.

Usage:
    tools/gen-synthetic-bids.py OUT --subjects 2000            # ~100k files, raw
    tools/gen-synthetic-bids.py OUT --variant fmriprep --subjects 500

Then:
    BIDSLAKE_TIMING=1 target/release/bidslake index -i OUT -o /tmp/synthetic.duckdb
"""

import argparse
import json
import os

# A confounds header of roughly the width fMRIPrep emits. The count is what makes
# the header expensive to carry per file, not the names.
N_CONFOUND_COLUMNS = 1800
N_CONFOUND_ROWS = 300


def touch(path):
    """Create an empty file. Imaging bytes are never read during indexing."""
    open(path, "w").close()


def write_raw(root, subjects, sessions, runs):
    sidecar = json.dumps({"RepetitionTime": 2.0, "EchoTime": 0.03, "TaskName": "rest"})
    events = "onset\tduration\ttrial_type\n" + "".join(
        f"{i * 2.0}\t1.0\t{'ab'[i % 2]}\n" for i in range(20)
    )
    n = 0
    for s in range(1, subjects + 1):
        sub = f"sub-{s:04d}"
        anat = os.path.join(root, sub, "anat")
        os.makedirs(anat, exist_ok=True)
        touch(os.path.join(anat, f"{sub}_T1w.nii.gz"))
        with open(os.path.join(anat, f"{sub}_T1w.json"), "w") as fh:
            fh.write('{"FlipAngle": 8}')
        n += 2
        for e in range(1, sessions + 1):
            ses = f"ses-{e:02d}"
            func = os.path.join(root, sub, ses, "func")
            os.makedirs(func, exist_ok=True)
            for r in range(1, runs + 1):
                base = f"{sub}_{ses}_task-rest_run-{r:02d}"
                touch(os.path.join(func, f"{base}_bold.nii.gz"))
                touch(os.path.join(func, f"{base}_sbref.nii.gz"))
                # One sidecar per run: the shape that makes a per-suffix bucket as
                # large as the dataset.
                with open(os.path.join(func, f"{base}_bold.json"), "w") as fh:
                    fh.write(sidecar)
                with open(os.path.join(func, f"{base}_events.tsv"), "w") as fh:
                    fh.write(events)
                n += 4
    return n


def write_fmriprep(root, subjects, sessions, runs):
    names = [f"confound_{i:04d}" for i in range(N_CONFOUND_COLUMNS)]
    confounds = "\t".join(names) + "\n"
    confounds += ("\t".join("0.1" for _ in names) + "\n") * N_CONFOUND_ROWS
    # The column dictionary fMRIPrep ships alongside; large, and read in full.
    confounds_json = json.dumps({n: {"Method": "generated", "Retained": True} for n in names})

    with open(os.path.join(root, ".bidsignore"), "w") as fh:
        fh.write("*_timeseries.tsv\n*_xfm.*\n")

    n = 1
    for s in range(1, subjects + 1):
        sub = f"sub-{s:04d}"
        anat = os.path.join(root, sub, "anat")
        os.makedirs(anat, exist_ok=True)
        touch(os.path.join(anat, f"{sub}_desc-preproc_T1w.nii.gz"))
        touch(os.path.join(anat, f"{sub}_from-MNI152_to-T1w_mode-image_xfm.h5"))
        n += 2
        for e in range(1, sessions + 1):
            ses = f"ses-{e:02d}"
            func = os.path.join(root, sub, ses, "func")
            os.makedirs(func, exist_ok=True)
            for r in range(1, runs + 1):
                base = f"{sub}_{ses}_task-rest_run-{r:02d}"
                touch(os.path.join(func, f"{base}_desc-preproc_bold.nii.gz"))
                touch(os.path.join(func, f"{base}_desc-brain_mask.nii.gz"))
                touch(os.path.join(func, f"{base}_boldref.nii.gz"))
                with open(os.path.join(func, f"{base}_desc-preproc_bold.json"), "w") as fh:
                    fh.write('{"RepetitionTime": 2.0}')
                with open(os.path.join(func, f"{base}_desc-confounds_timeseries.tsv"), "w") as fh:
                    fh.write(confounds)
                with open(os.path.join(func, f"{base}_desc-confounds_timeseries.json"), "w") as fh:
                    fh.write(confounds_json)
                n += 6
    return n


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("out", help="directory to create the dataset in")
    ap.add_argument("--variant", choices=("raw", "fmriprep"), default="raw")
    ap.add_argument("--subjects", type=int, default=500)
    ap.add_argument("--sessions", type=int, default=2)
    ap.add_argument("--runs", type=int, default=6)
    args = ap.parse_args()

    os.makedirs(args.out, exist_ok=True)
    description = {"Name": f"synthetic-{args.variant}", "BIDSVersion": "1.8.0"}
    if args.variant == "fmriprep":
        description["DatasetType"] = "derivative"
        description["GeneratedBy"] = [{"Name": "fMRIPrep", "Version": "23.0.0"}]
    with open(os.path.join(args.out, "dataset_description.json"), "w") as fh:
        json.dump(description, fh)

    writer = write_raw if args.variant == "raw" else write_fmriprep
    n = writer(args.out, args.subjects, args.sessions, args.runs)
    print(f"{n} files under {args.out} ({args.variant})")


if __name__ == "__main__":
    main()
