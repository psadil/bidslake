# Issues to file

Work items with an owner in the code rather than an unsettled design question — those live in the
`## Open Issues` section of the ADR that owns the subject. Each entry below is self-contained: title,
then a body that names the files it touches. Delete an entry once it is filed.

## Stream `get()` instead of materializing the whole frame

`BidsLake.get` in `crates/bidslake-py/python/bidslake/layout.py` is typed `Iterator[BidsFile]`, but it
builds the entire Arrow-IPC buffer and a Polars frame before yielding the first row, so its laziness
is cosmetic and its memory cost is the whole result set. The docstring says so; the fix is to make the
signature true. When the PyO3 PyCapsule stream bridge is available in
`crates/bidslake-py/src/lib.rs`, hand Arrow batches across the boundary and yield per batch, so
`get()` holds one batch at a time. Cover it with a test that iterates a partial result and asserts the
frame was never built.

## Widen CI past a single Linux runner

`.github/workflows/ci.yml` runs four jobs — rust (fmt, clippy, test), python (pytest, ty), docs
(rustdoc, `cargo test --doc`, mdbook, link check) and the codegen-drift guard — all on one
`ubuntu-latest` runner at one Python and one Rust version. Four additions, roughly in value order: an
OS/Python/Rust matrix; a scheduled run of the `#[ignore]`d whole-corpus tests
(`crates/bidslake/tests/smoke_bids_examples.rs`, `crates/bidslake/tests/tabular_coverage.rs`), which
nothing runs today; benchmark-regression tracking over `cargo bench` in `bidslake` and
`bids-validator-rs`; and coverage reporting.

## Benchmark a tree wide enough for the tabular path to show

`crates/bidslake/benches/ingest.rs` ingests `ds001`, `ds002`, `ds114` and `ds108`. None of the four
ships a `scans.tsv` or a `sessions.tsv`, and none has a header wider than ~11 columns, so a regression
in the batched tabular path costs nothing any benchmark in the repo can see. Cheapest first steps: add
the vendored `third_party/bids-examples/7t_trt` and `synthetic`, the only corpus trees carrying
`scans.tsv`, `sessions.tsv` and a ~95-column header; drive `tools/gen-synthetic-bids.py` — a scale
generator nothing outside its own docstring calls — into a wide-confounds case; and expose a
`timing::snapshot()` from `crates/bidslake/src/timing.rs` so counter assertions (one tabular group per
batched read, one undeclared statement per distinct name) fail under `cargo test` rather than needing
criterion baselines.

## Port synthetic derivative-tree generation into `tools/`

`tools/gen-synthetic-bids.py` generates raw BIDS at a chosen scale, which is what the performance work
on the walk and the sidecar paths needed. The equivalent generators for fMRIPrep and FreeSurfer
*derivative* trees — the shapes that exercise overlays, term maps and the adapter read paths, and that
`third_party/bids-examples` reaches only at toy size — exist only in a consumer repo's `spikes/`
directory, which no reader of this repo can open. Port them beside `gen-synthetic-bids.py` with the
same scale parameters and the same empty-file trick for imaging data, then point the fixtures at them:
`crates/bidslake/tests/test_adapter_freesurfer.rs` and `crates/bidslake/tests/test_overlay.rs` build
their trees by hand, and the wide-confounds benchmark case above needs one.

## Document the public surface of `bids-validator-rs`

`crates/bids-validator-rs/src/lib.rs` opens with `#![allow(missing_docs)]` over 287 undocumented public
items. `missing_docs` is `warn` workspace-wide in the root `Cargo.toml` and CI runs clippy with
`-D warnings`, so that one attribute is the whole of what keeps the crate green. Document the items and
delete the attribute; the workspace lint comment names removing it as the entirety of this task.

## Document the public surface of `hed-validator-rs`

The same task for `crates/hed-validator-rs/src/lib.rs`, whose `#![allow(missing_docs)]` covers 176
undocumented public items. Separate from the `bids-validator-rs` one: different crate, different
vocabulary, and either can land alone.

## Extend the bundled overlays to what they still skip

The five overlays under `crates/bids-schema/data/overlays/` cover the common outputs; several known
files still route nowhere. On `third_party/bids-examples/ds000001-fmriprep`,
`*_desc-MELODIC_mixing.tsv` and `*_AROMAnoiseICs.csv` index as `skipped`. MRIQC's group TSVs are
undeclared, and they are the only route to *dataset-level* IQM summaries — the per-image IQMs already
populate from `sub-…_T1w.json`/`_bold.json`, 475 records on `ds001761-mriqc`. QSIPrep has further QC
files. Column *values* are only lightly exercised because the bids-examples confounds files are empty:
check the declared names against a dataset carrying real confound data when one is available, which
would also let the corpus carry the row-alignment assertion that
`crates/bidslake/tests/test_overlay.rs::adapter_keys_confounds_rows_to_every_image_of_the_run` makes
against a synthetic fixture.

## Add `emit-types --from-db`

`python -m bidslake.stubgen` (`crates/bidslake-py/python/bidslake/stubgen.py`) regenerates the typed
column surface from a catalog's stored `effective_schema` and is the recommended path. For cargo-only
workflows, add a `--from-db <db>` mode to `crates/bidslake-py/src/bin/emit_types.rs` that reads the same
stamped schema out of `bidslake_schema` rather than the vendored one.

## Keep the `bidslake_*` meta tables out of the generated typed surface

`bidslake_meta` and `bidslake_schema` are internal provenance tables, and both appear in the generated
`COLUMNS` map and `C` class in `crates/bidslake-py/python/bidslake/schema/_generated.py`. Filter them
in the emitter — `crates/bidslake-py/src/bin/emit_types.rs`, and `stubgen.py` for the `--from-db` path
— so the typed surface offers only tables a query means. The regenerated module ships in the same
commit; CI's codegen-drift job compares the two.

## A reader for headerless numeric matrices

FSL writes several: `filtered_func_data.ica/melodic_mix` and `mc/prefiltered_func_data_mcf.par` are
whitespace-delimited with no header, and `melodic_mix`'s column count varies per run. The `csv` reader
assumes tabs and schema-declared columns, so the `feat` adapter catalogs these files as paths rather
than reading them. Widening the recording path does not reach them: a BIDS headerless recording is
*derived* — `crates/bidslake/src/schema/recording.rs` finds a tabular suffix whose column names the
schema locates outside the file, in the sidecar `Columns` or an associated `_channels.tsv` — and no
BIDS rule describes an FSL matrix. The `feat` term map projects `mcf.par` to `datatype=func,
suffix=timeseries, desc=motion`, and nothing declares where its column names live, so a reader is the
declarative hook it needs: add one to `crates/bidslake/src/readers/mod.rs`, register it in
`default_readers`, and select it from `crates/bids-schema/data/ingestion/feat.json` so the mixing
matrix and the motion parameters become tables.

## Author layouts for fMRIPrep, MRIQC and QSIPrep

`crates/bids-schema/data/layouts/feat.json` is the only layout, so code producing files in the other
three producers' conventions hardcodes its paths. Each is one layout document plus its `Examples`,
validated against `crates/bids-schema/data/layout-metaschema.json`; the `Examples` bind the write
direction against the read side at load time, and the round-trip check does the rest. The three have
overlays under `crates/bids-schema/data/overlays/` to name their vocabulary from, and no term map yet.
ADR 0002 (`docs/adr/0002-adapters-and-layouts.md`) records the gap.

## Give the four under-specified `feat` roles their `Entities`

Of the 23 roles in `crates/bids-schema/data/layouts/feat.json`, four declare no `Entities`:
`highres`, `example_func`, `classification` and `classification_by_rater`. Add them to the layout and
to `crates/bids-schema/data/term-maps/feat.json` in one change — the round trip checks each against
the other — so a written FEAT tree is better classified when it is re-indexed. This does not make
roles usable as source selectors, which is a structural limit rather than a gap
(`docs/adr/0002-adapters-and-layouts.md`).

## Give ADR 0002's ingest percentages a corpus and a date

The two percentages in `docs/adr/0002-adapters-and-layouts.md`'s Rationale name only the query
shape — no corpus, catalog size, build profile or date — yet they are the record's sole quantitative
support for a load-bearing decision. Every other measurement in the set says what was measured on
what (`docs/adr/0004-undeclared-column-policy.md` is the model: "Profiled 2026-07 on 12 fMRIPrep
derivative datasets, 2 subjects each, 48 confounds files"). Re-run the measurement and state its
basis, or drop the figures and keep the qualitative claim.
