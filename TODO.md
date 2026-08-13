# Follow-ups

Deferred / optional items surfaced by the July 2026 design sweep but left out of the
remediation pass (see the finding ids in parentheses). Recorded here for later; not filed
as issues. Roughly ordered by value.

- [ ] **An inherited `.bval`/`.bvec` applies to many images, and nothing models that.**
  Surfaced while adding the `diffusion` → `file_registry` foreign key (ADR 0006), which turned a
  silent bug loud. `process_diffusion_file` derives the image a gradient set belongs to by
  swapping the extension on its stem — `sub-01_dwi.bval` → `sub-01_dwi.nii.gz` — which is right
  only for a gradient file sitting beside its image. BIDS inheritance also allows one at a
  *higher* level, applying to every image below it: `ds114` ships a root-level `dwi.bval` and
  `dwi.bvec`, whose synthesized `dwi.nii.gz` is nothing on disk. Before the FK, that wrote
  `diffusion` rows keyed to a file the dataset does not contain; now those gradients are
  **skipped**, so an inherited gradient set is not queryable at all (the files themselves are
  still in `file_registry`, under their own paths).

  Neither behaviour is right, and the shape is the problem: `diffusion` is keyed
  `(file_id, volume_idx)`, one row per volume *of one image*, so a gradient set shared by N
  images has no single referent to key on. Resolving it needs a **many-to-many** relation
  between a gradient file and the images that inherit it — the same shape
  `file_associations` has for `IntendedFor`, and the same resolution `SidecarIndex` already
  performs for JSON sidecars (`(dataset_id, suffix, directory)` with an entity-subset match).
  The obvious route is to reuse that: resolve the gradient file to its set of images the way
  inheritance resolves a sidecar, then either store `diffusion` per (image, volume) with the
  values duplicated, or key it to the *gradient* file and join through an association table.
  Worth deciding which before implementing; the second keeps one row per gradient volume but
  makes every read a join.

- [ ] **Genuine lazy `get()` streaming** (`py-04`). `get()` is typed `Iterator[BidsFile]` but
  materializes the whole Arrow-IPC buffer + Polars frame first, so its laziness is cosmetic
  (now documented in the docstring). When the PyO3 PyCapsule stream bridge lands
  (`crates/bidslake-py/src/lib.rs`), stream Arrow batches so `get()` is O(1) memory.

- [ ] **Fully convert `db.rs`/`dynamic.rs` to `anyhow`** (`eh-05` optional). Beyond the
  call-site `.context()` already added, push table/path context inside the write layer. Requires
  rewriting the two manual `duckdb::Error::ToSqlConversionFailure` constructions in
  `crates/bidslake/src/schema/dynamic.rs`.

- [x] **First-writer-wins `dataset_description` rows** (`eh-04`). Resolved by
  [ADR 0005](docs/adr/0005-multi-root-datasets.md) §5: a real `dataset_description.json` now
  upserts (`Schema::insert_or_replace`, a `guard: bool` on `build_insert_sql`), so a description
  added or corrected since the first index reaches the catalog on re-index. The synthesized row
  for a dataset that has none keeps its `WHERE NOT EXISTS` guard so it can never shadow a real
  one, and only the *shallowest* description is written — with `OR REPLACE`, a derivative's
  would otherwise overwrite its parent's.

- [ ] **Recording bare-table const consolidation** (`pat-02`). `crates/bidslake/src/schema/dynamic.rs`'s
  hardcoded `["motion", "stim"]` bare-table list could fold into the shared recording descriptor
  if that descriptor is promoted to a shared location and carries a "bare" flag.

- [ ] **Validator double-compute of datatype/modality/entities** (`dup-04`). Optional, low value:
  `crates/bids-validator-rs/src/context.rs` derives the core selector fields once for its struct
  and again via `build_file_context`. Fixing it re-introduces hand-assembly or needs a
  precomputed-inputs `build_file_context` variant, to save three cheap in-memory calls.

- [x] **`dataset_id` conflates the catalog partition with the ingest root.** Resolved by
  [ADR 0005](docs/adr/0005-multi-root-datasets.md) and
  [ADR 0006](docs/adr/0006-file-registry.md). `dataset_roots(dataset_id, root_uri)` holds one
  row per root, so subject-sharded pipeline output is one dataset with N roots and needs no
  flags; `root_uri` is off `dataset_description` entirely. The per-root identity that
  `(dataset_id, file_path)` could no longer supply became `file_id`, a hash of
  `(dataset_id, root_uri, file_path)`, which every file-keyed table now keys on. The
  no-`root_id`-label decision is recorded in ADR 0005 §2.

- [ ] **Enforced referential integrity for the concept columns?** `all_files.sub`/`ses` cannot be
  foreign keys as built: they are select items of a *view* now (ADR 0006 §3) rather than the
  generated columns of a table, and DuckDB refuses a foreign key on either. Making them keys
  would mean materializing the concepts onto `file_registry` — forfeiting the property that a
  term-map or overlay fix changes existing answers with no re-index, which is exactly what the
  view bought — plus reconciling `sub` (`01`) against `participants.participant_id` (`sub-01`),
  and accepting that a file whose subject is absent becomes a hard ingest error rather than a
  queryable fact. Currently the invariant is asserted by test instead
  (`test_adapter_freesurfer.rs`: every registry `sub` resolves to a `participants` row).

- [ ] **CI enhancements**. The initial `.github/workflows/ci.yml` covers fmt/clippy/test, the
  Python suite, and the codegen drift guard on a single Linux runner. Later: an OS/Python/Rust
  matrix, benchmark-regression tracking (`cargo bench` in `bidslake` and `bids-validator-rs`), a
  scheduled run of the `#[ignore]` whole-corpus smoke test, and code coverage.

- [ ] **The benchmarks cannot see the tabular wins.** `benches/ingest.rs` runs
  `ds001`/`ds002`/`ds114`/`ds108`, none of which has a `scans.tsv`, a `sessions.tsv`, or a header
  wider than ~11 columns — so nothing in the repo would notice the batched tabular path
  regressing. Cheapest first steps: add the already-vendored `7t_trt` and `synthetic` (the only
  corpus trees with `scans.tsv`, `sessions.tsv` and a ~95-column header); wire
  `tools/gen-synthetic-bids.py` — a scale generator referenced nowhere outside its own docstring —
  into a wide-confounds case; and expose a `timing::snapshot()` so counter assertions
  (`tabular_groups == 1`, undeclared statements == distinct names) can fail in the existing
  `cargo test` CI rather than needing criterion baselines.

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

- [ ] **A narrower later run silently strips concept columns off `all_files`.** The other edge
  of docs/adr/0006 §3's `CREATE OR REPLACE VIEW`. Widening is retroactive, which is the win;
  narrowing is retroactive too. Indexing plain BIDS into a catalog an adapter built redefines
  the view without the concepts the wider run added and without the `COALESCE` over
  `projected`. Measured: a catalog built `--adapter fmriprep --adapter freesurfer` then
  indexed again with neither loses `from`/`to`/`mode`/`parc`, and its FreeSurfer rows read
  `datatype` NULL. Nothing is destroyed — the rows and their `projected` JSON are untouched,
  and re-running wide restores every answer — but the catalog misleads until someone does, and
  `check_registry_shape` cannot see it because no *physical* column is missing. Two candidate
  fixes: make the view definition additive by unioning the incoming concept set with what the
  catalog's stamped `bidslake_schema` already records, or refuse a narrowing run the way a
  missing `projected` is refused. The first is better for users and needs the stamp to be read
  back at `create_tables` time, which nothing does today.
  `test_registry_shape.rs::narrowing_is_allowed` currently asserts only that no error is
  raised, and should assert whichever behaviour is chosen.

- [ ] **Extend the `file_registry` foreign key to the per-row tables.** docs/adr/0006 §4 gives
  `scans`, `sidecars` and `diffusion` a real `FOREIGN KEY (file_id) REFERENCES
  file_registry(file_id)`; the 22 per-row tables (`events`, `*_channels`, `motion`, `physio`,
  the adapter reader tables) reference it by convention only. The FK on `diffusion` caught a
  real dangling-reference bug immediately, which is the argument for extending it — but each
  per-row table is written by a bulk `INSERT ... SELECT` from `read_csv`, so the per-row
  constraint check has to be measured against the batched-ingest benchmark before it is
  imposed. Expect it to surface latent bugs of the same shape as the `ds114` one, so budget
  for those rather than treating it as a one-line change. `file_associations` is a separate
  case and cannot take one as it stands: `target_file_id` is nullable by design.

- [ ] **Rebuild the file registry when a later run needs a wider `projected`**. Mostly closed by
  [ADR 0006](docs/adr/0006-file-registry.md) §3: the concept columns moved onto the `all_files`
  *view*, which is emitted `CREATE OR REPLACE`, so a later run whose overlay is wider simply
  redefines them — retroactively, for rows already stored — and `check_registry_shape` no longer
  has anything to refuse there. What remains is `projected`, a physical `JSON` column on
  `file_registry` that a catalog created without a term map cannot gain, so the "name every
  adapter the catalog uses" remedy still applies to that one column. The fuller fix is the same
  as before — rebuild the table in place (new table, copy, swap), which `compact.rs` has most of
  the machinery for — but the blast radius is now larger, not smaller: 25 tables carry a foreign
  key to `file_registry`, not just `sidecars`. Regression tests: `test_registry_shape.rs`.

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
