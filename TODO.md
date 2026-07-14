# Follow-ups

Deferred / optional items surfaced by the July 2026 design sweep but left out of the
remediation pass (see the finding ids in parentheses). Recorded here for later; not filed
as issues. Roughly ordered by value.

- [ ] **Genuine lazy `get()` streaming** (`py-04`). `get()` is typed `Iterator[BidsFile]` but
  materializes the whole Arrow-IPC buffer + Polars frame first, so its laziness is cosmetic
  (now documented in the docstring). When the PyO3 PyCapsule stream bridge lands
  (`crates/bidslake-py/src/lib.rs`), stream Arrow batches so `get()` is O(1) memory.

- [ ] **Push `materialize` scheme-branching behind the trait** (`abstraction-01` secondary).
  Have each `BidsFileSystem` impl return a `read_csv`-ready source string (absolute local path,
  or `s3://` URL) so the `starts_with("s3://")` branch at the call sites can be deleted;
  optional rename `materialize` → `read_csv_source`.

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

- [ ] **Concurrent Rust-side reads**. The known-deferred ingest perf lever from the prior
  performance sweep (the sidecar/tabular header reads are prefetched concurrently, but the
  per-file Rust-side reads are still sequential).

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

- [ ] **Auto-relax `.bidsignore` under `--overlay`?** Consider having an overlay imply
  `--no-bidsignore` (or selectively un-ignore only schema-recognized files), so the common case
  needs one flag, not two. Currently explicit.

- [ ] **YAML overlay authoring**. Overlays are JSON-only; accept `.yaml`/`.yml` (parse to `Value`
  before merge) behind an optional `yaml` cargo feature.

- [ ] **Rust `emit-types --from-db`**. The Python `stubgen` is the recommended path; optionally add
  a `--from-db <db>` mode to the `emit-types` bin for cargo-based workflows.

- [ ] **Consider filtering `bidslake_*` meta tables** from the generated `COLUMNS`/`C` typed surface
  (they are internal provenance tables; `bidslake_meta`/`bidslake_schema` currently appear there).

- [ ] **Batched-insert crash on empty header columns** (pre-existing, unrelated to overlays). A TSV
  with a trailing tab (an empty-string column name) makes the batched insert emit
  `json_object('', raw."")`, a "zero-length delimited identifier" parser error that drops the file
  (seen as a warning on `ds001` events). The single-file path tolerates it; harden the batched path.
