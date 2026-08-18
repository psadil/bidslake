# bidslake-synth

Synthetic BIDS and derivative trees, built from the bundled schemas rather than from format
strings.

```bash
cargo run -p bidslake-synth -- /tmp/synth \
    --producer raw --producer fmriprep --producer freesurfer --producer feat \
    --subjects 50 --preset-a2cps-melodic --print-index
```

Each producer writes its own subdirectory under the output path, and each prints the
`bidslake index` command its tree needs — a FreeSurfer root needs `--adapter freesurfer`, an
fMRIPrep root with a `.bidsignore` needs `--no-bidsignore`, and a tree with no instructions is a
tree nobody indexes correctly.

## What it is for

**Benchmarking.** `bids-examples` tops out around 2,400 files and its widest tabular header is
about eleven columns, so the costs that dominate at 100k files and 1,841 columns are invisible
there. `cargo bench -p bidslake-synth` measures both.

**Experimentation.** A tree of a chosen shape, on demand, with nothing to download.

**Authoring an adapter bundle.** A layout, a term map and an ingestion fragment together describe
a tree that does not exist yet. Point the generator at them and it materializes that tree, and
`--explain` reports what each path classifies as and how ingest would dispose of it:

```bash
cargo run -p bidslake-synth -- /tmp/probe \
    --layout ./my-layout.json --term-map ./my-term-map.json \
    --ingestion ./my-ingestion.json --explain
```

That is the fastest way to find out that a hand-written PCRE claims one path and not its sibling.
`--explain` writes nothing and exits nonzero when a role renders a path nothing claims.

**Not** correctness testing. Those fixtures are hand-written, in `crates/bidslake/tests/`, where
the expected values are written out by a human and each file carries the reason it exists.

## How much of it tracks the schema

| | tracks the schema? |
|---|---|
| Raw BIDS paths — `rules.files.raw`, `objects.entities`, `rules.entities` order | yes |
| Every tabular body — `rules.tabular_data` + `objects.columns` | yes |
| Sidecar bodies — `rules.sidecars` + `objects.metadata` | yes |
| Layout-backed tree paths — a layout's `Roles` and `Examples` | yes |
| Derivative paths for pipeline-invented vocabulary | **no** |

The last row looks like a gap and is a fact about the documents. No bundled overlay touches
`rules.files` — they carry `objects.*` and `rules.tabular_data` only — and base
`rules.files.deriv` declares none of `timeseries`, `xfm`, `boldref`, `mixing`, `components` or
`classification`. There is nothing to enumerate, so each producer carries a small path recipe, and
one test asserts that every suffix an overlay adds is emitted by *some* producer: a newly declared
suffix turns the build red rather than becoming a silent hole in the benchmark.

## The two axes

Scale is uniform by construction — every subject gets the same files, so the file count is exactly
linear in `--subjects` and a superlinear ingest cost shows up as a bend in a one-at-a-time sweep:

```
--subjects --sessions --runs --tasks --spaces
--confound-columns --confound-rows
--preset-a2cps-melodic     # 1841 × 450, the measured shape of a real fMRIPrep 25.2.5 file
```

Fidelity breaks that by design, so it is opt-in, named one hazard at a time, and recorded in the
manifest. A benchmark number quoted without saying which hazards were on is not a number.

```
--hazards compound-ext,extensionless,symlink,empty-dir,dotted-dir,
          ragged,quoted-tsv,zero-byte,loose-artifacts
--hazards all
```

Naming them individually rather than taking a single `--realistic` is what lets a benchmark turn
on exactly one and attribute the difference to it.

## Two things worth knowing

**Imaging files are not empty.** They carry a 348-byte NIfTI-1 header and eight voxels, gzipped
without compression, at about 380 bytes each — a hundred-thousand-file tree grows by roughly forty
megabytes. An empty `.nii.gz` is four validator errors, and the bar this crate holds itself to is
that a generated raw tree passes `bids-validator-rs` cleanly. The gzip is stored rather than
deflated because `NIFTI_TOO_SMALL` compares the *on-disk* byte count against 348, so a
well-compressed valid volume is reported as too small to hold the header it demonstrably holds.

**`--bidsignore` is off by default**, which is the opposite of faithful on purpose. The file
fMRIPrep really writes hides `*_xfm.*` and `*_timeseries.tsv` — exactly the files an fMRIPrep tree
is generated to exercise — so turning it on means an ingest needs `--no-bidsignore` to see
anything.
