# Follow-ups

Deferred / optional items surfaced by the July 2026 design sweep but left out of the
remediation pass (see the finding ids in parentheses). Recorded here for later; not filed
as issues. Roughly ordered by value.

- [ ] **Genuine lazy `get()` streaming** (`py-04`). `get()` is typed `Iterator[BidsFile]` but
  materializes the whole Arrow-IPC buffer + Polars frame first, so its laziness is cosmetic
  (now documented in the docstring). When the PyO3 PyCapsule stream bridge lands
  (`crates/bidslake-py/src/lib.rs`), stream Arrow batches so `get()` is O(1) memory.

- [ ] **Fully convert `db.rs`/`dynamic.rs` to `anyhow`** (`eh-05` optional). Beyond the
  call-site `.context()` already added, push table/path context inside the write layer. Requires
  rewriting the two manual `duckdb::Error::ToSqlConversionFailure` constructions in
  `crates/bidslake/src/schema/dynamic.rs`.

- [ ] **First-writer-wins rows under the `WHERE NOT EXISTS` guard** (`eh-04` note). Whichever of
  the implicit participant/session insert vs the `participants.tsv`/`sessions.tsv` ingest runs
  first wins — so a bare implicit row can shadow the richer tabular row. `dataset_description` now
  has the same shape: the synthesized `{dataset_id, root_uri}` row for adapter datasets is ordered
  *after* the walk so it can never shadow a real `dataset_description.json` **within** a run, but
  across runs into one database the table is still first-writer-wins on `dataset_id` (no upsert),
  so re-ingesting a dataset whose description was added later will not refresh it. A real, distinct
  correctness concern worth its own investigation (an upsert/`ON CONFLICT DO UPDATE` path).

- [ ] **Recording bare-table const consolidation** (`pat-02`). `crates/bidslake/src/schema/dynamic.rs`'s
  hardcoded `["motion", "stim"]` bare-table list could fold into the shared recording descriptor
  if that descriptor is promoted to a shared location and carries a "bare" flag.

- [ ] **Validator double-compute of datatype/modality/entities** (`dup-04`). Optional, low value:
  `crates/bids-validator-rs/src/context.rs` derives the core selector fields once for its struct
  and again via `build_file_context`. Fixing it re-introduces hand-assembly or needs a
  precomputed-inputs `build_file_context` variant, to save three cheap in-memory calls.

- [ ] **CI enhancements**. The initial `.github/workflows/ci.yml` covers fmt/clippy/test, the
  Python suite, and the codegen drift guard on a single Linux runner. Later: an OS/Python/Rust
  matrix, benchmark-regression tracking (`cargo bench` in `bidslake` and `bids-validator-rs`), a
  scheduled run of the `#[ignore]` whole-corpus smoke test, and code coverage.

## Schema augmentation (overlays)

Follow-ups from the overlay feature (see `docs/adr/0001-schema-augmentation-overlays.md`).
Landed and verified: the core; all three bundled overlays (fMRIPrep, MRIQC, QSIPrep — authored
and metaschema-valid); `index --no-bidsignore` (walk past a pipeline's `.bidsignore`, without
which overlays are inert on real derivative datasets — validated on `ds000001-fmriprep`);
`schema --diff`/`index --dry-run`; dataset-embedded overlay auto-discovery; the Python runtime
accessors; and the opt-in `python -m bidslake.stubgen`. Remaining follow-ups:

- [ ] **Grow bundled-overlay coverage**. The three overlays cover the common outputs; extend them
  as needs arise — e.g. the fMRIPrep overlay does not yet capture `*_desc-MELODIC_mixing.tsv` or
  `*_AROMAnoiseICs.csv` (they show as `skipped` on `ds000001-fmriprep`); MRIQC group TSVs; more
  QSIPrep QC files. Column *values* are only lightly validated (the bids-examples confounds files
  are empty) — check names against a dataset with real confound data when one is available.
  (MRIQC's *per-image* IQMs no longer need the group TSVs: a sidecar whose data file the dataset
  never ships is now promoted to a record of its own, so the overlay's typed IQM columns populate
  straight from `sub-…_T1w.json`/`_bold.json` — validated on `ds001761-mriqc`, 475 records.
  The group TSVs remain the only route to *dataset-level* IQM summaries.)

- [ ] **Auto-relax `.bidsignore` under `--adapter`?** Consider having an adapter imply
  `--no-bidsignore` (or selectively un-ignore only schema-recognized files), so the common case
  needs one flag, not two. Currently explicit — and now the sharpest edge left for MRIQC, whose
  `.bidsignore` hides the very `*_T1w.json`/`*_bold.json` its metrics live in, so
  `--adapter mriqc` alone still yields an empty catalog. Interim: an ingest that indexes no data
  files while `.bidsignore` is in force now says so, instead of reporting success over an empty
  database (see `promote_orphan_sidecars`' call site in `bids.rs`).

- [x] **Cross-dataset association** — landed at the *dataset* level, not by entity guessing
  (`docs/adr/0003`). Datasets declaring the same `SourceDatasets` are co-derivatives
  (`shares_source`, resolved by the `dataset_relations` view); `lake.related_datasets(id, relation)`
  gives a consumer the sound relation, within which it can then match files by entity. Validated on
  `ds001761-fmriprep`/`-mriqc`. **Remaining:** the precise *file*-level link via the BIDS `Sources`
  metadata field (a `target_dataset_id` on `file_associations`, BIDS-URI resolution through
  `DatasetLinks`) — deferred because no producer we have emits `Sources` (MRIQC emits neither it nor
  the deprecated `RawSources`; an issue has been filed with nipreps/mriqc). See ADR 0003 §6.

- [ ] **YAML overlay authoring**. Overlays are JSON-only; accept `.yaml`/`.yml` (parse to `Value`
  before merge) behind an optional `yaml` cargo feature.

- [ ] **Rust `emit-types --from-db`**. The Python `stubgen` is the recommended path; optionally add
  a `--from-db <db>` mode to the `emit-types` bin for cargo-based workflows.

- [ ] **Consider filtering `bidslake_*` meta tables** from the generated `COLUMNS`/`C` typed surface
  (they are internal provenance tables; `bidslake_meta`/`bidslake_schema` currently appear there).

- [x] **Batched-insert crash on empty header columns** (pre-existing, unrelated to overlays). A TSV
  with a trailing tab (an empty-string column name) made the batched insert emit
  `json_object('', raw."")`, a "zero-length delimited identifier" parser error that dropped the
  file — and, since the batched path has no per-file fallback, every other file in its header group
  too. Both SQL builders now filter empty column names out of the `other_data` extras. Regression
  test: `tabular_row_order::trailing_tab_header_still_ingests` (the vendored `ds001` no longer
  carries such a header, so the test ships its own fixture).

## Derivation layer

- [ ] **Promote bindings to a stamped artifact**. `bidslake.binding` is deliberately typed
  Python and not a JSON document: the dataclasses are a 1:1 match for what such a document
  would hold, so promoting is writing a serializer, but the shape has not earned its
  metaschema yet. ADR 0002 §1 records why the `x_bidslake` prototype was rejected — "one
  artifact, three concerns, no standards path, bespoke parser" — and a work-unit language
  invented before real pipelines exercise it would repeat that.
  **Trigger:** two or three pipelines have used bindings without the shape moving. Then add
  `data/bindings/*.json` + a hand-written `binding-metaschema.json`, stamp it as
  `bidslake_bindings` beside the overlay/term-map/ingestion stamps, and write the ADR. Until
  then, changing a field is a Python edit rather than a schema migration — which is the point
  of waiting.

- [ ] **Rebuild the file registry when a later run needs a wider one**. Tables are created
  `IF NOT EXISTS`, so `scans` keeps the shape of the run that created it while datasets
  accumulate across runs (ADR 0002 §3). `BidsDb::check_registry_shape` turns the resulting
  silent column loss into an error whose remedy is "name every adapter the catalog uses on
  every run", which makes order irrelevant — but it is a workaround. The fuller fix is to
  rebuild `scans` in place (new table, copy physical columns, swap), which `compact.rs`
  already has most of the machinery for; the complication is that `sidecars` carries a
  foreign key to `scans`. Regression tests: `test_registry_shape.rs`.

- [ ] **A reader for headerless numeric matrices**. FSL writes several
  (`filtered_func_data.ica/melodic_mix`, `mc/prefiltered_func_data_mcf.par`): whitespace-
  delimited, no header, and in `melodic_mix`'s case a column count that varies per run. The
  `csv` reader assumes tabs and schema-declared columns, so the `feat` adapter catalogs these
  rather than reading them. A `ContentReader` would make the mixing matrix and motion
  parameters queryable as tables instead of paths.

- [ ] **Stamp a layout when one is used to *produce* a dataset**. Overlays, term maps and
  ingestion fragments are all recorded in `bidslake_*` tables, so a catalog records how its
  files were read (ADR 0001 §4). A layout is not, and today that is right — it is consulted
  by whatever writes a tree, before there is a catalog to stamp. But a FEAT tree's layout is
  provenance about what those files were *meant to be*, and nothing currently records it.
  Blocked on there being a producing path inside bidslake at all; revisit alongside the
  derivation-record work above rather than on its own.

- [ ] **Layouts for the other bundled producers**. `data/layouts/feat.json` is the first;
  fMRIPrep, MRIQC, and QSIPrep have term maps or overlays but no write direction, so code
  producing files in their conventions still hardcodes paths. Each is a layout document
  plus its `Examples`; the round-trip check does the rest.
