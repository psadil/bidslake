# Follow-ups

Deferred / optional items surfaced by the July 2026 design sweep but left out of the
remediation pass (see the finding ids in parentheses). Recorded here for later; not filed
as issues. Roughly ordered by value.

- [ ] **`*_channels.tsv` should declare `describes` too.** The `channels` association exists
  and the table is ordered, so `{ "association": "channels", "axis": "channel", "view": … }`
  would give the same re-keying every other per-row table now gets. Deferred only so that
  rewiring the positional channel-name lookup in `bids.rs` (`SELECT name FROM
  motion_channels … ORDER BY row_idx`, the one place row order is load-bearing *inside*
  bidslake) onto the view is a deliberate change rather than a side effect. `*_electrodes`
  and `coordsystem` are the multi-association case — several targets per source, so
  `(file_id, channel_idx)` would not be unique — which is permitted but wants a look first.

- [ ] **Genuine lazy `get()` streaming** (`py-04`). `get()` is typed `Iterator[BidsFile]` but
  materializes the whole Arrow-IPC buffer + Polars frame first, so its laziness is cosmetic
  (now documented in the docstring). When the PyO3 PyCapsule stream bridge lands
  (`crates/bidslake-py/src/lib.rs`), stream Arrow batches so `get()` is O(1) memory.

- [ ] **Enforced referential integrity for the concept columns?** `all_files.sub`/`ses` cannot be
  foreign keys as built: they are select items of a *view* now (ADR 0006 §3) rather than the
  generated columns of a table, and DuckDB refuses a foreign key on either. Making them keys
  would mean materializing the concepts onto `file_registry` — forfeiting the property that a
  term-map or overlay fix changes existing answers with no re-index, which is exactly what the
  view bought — plus reconciling `sub` (`01`) against `participants.participant_id` (`sub-01`),
  and accepting that a file whose subject is absent becomes a hard ingest error rather than a
  queryable fact. Currently the invariant is asserted by test instead
  (`test_adapter_freesurfer.rs`: every registry `sub` resolves to a `participants` row).

- [ ] **`file_id` is 64-bit, and that has a ceiling worth revisiting past ~10⁸ files.** The key
  moved from a 128-bit `HUGEINT` to a `UBIGINT` because `HUGEINT` does not survive the trip to
  Python: the Arrow bridge hands it over as `Decimal128(38, 0)`, whose maximum is `10^38 - 1`
  against `HUGEINT`'s `2^127 - 1 ≈ 1.7 × 10^38`, so 41% of the id space was outside its own
  declared type and any attempt to rebuild a frame from such a value raised. Widening was not
  available — polars caps decimal precision at 38 because its `Decimal` is Decimal128-backed.

  The cost is collision resistance. By the birthday bound a catalog of `n` files collides with
  probability `≈ n² / 2^65`: 3 × 10⁻⁸ at a million files, 3 × 10⁻⁶ at ten million, 3 × 10⁻⁴ at a
  hundred million. A million-file catalog — roughly a 10,000-run study counting its FreeSurfer
  trees — sits below the rate at which the disk under it corrupts a block, so this is the right
  trade today. It stops being obviously right somewhere past 10⁸.

  **A collision would be silent**, which is the part that decides when to act on this. `file_id`
  is a primary key with replace-on-conflict, so two files sharing one would quietly become one
  row rather than an error. Making it loud is cheap and self-contained: `file_registry` already
  stores `(dataset_id, root_uri, file_path)` beside the id, so a collision is exactly "this id is
  present with a different triple" — one join against the upsert stage before the
  `INSERT OR REPLACE`. Worth doing before anyone points bidslake at a catalog of that size,
  rather than widening the key again.

- [ ] **`verify` cannot detect a rewrite that preserved size and mtime.** It compares the
  registry's `size_bytes`/`mtime_ns` against the tree, which catches a file that was deleted,
  truncated, replaced or rewritten — the failures a derivative tree actually suffers — and
  misses a forgery, or a second write inside one timestamp tick on a filesystem with coarse
  mtime. A checksum column would close it and is deliberately not there: hashing means
  *reading* every file, which is a different order of cost from stat-ing one, and an index
  that read every byte of a study to build itself would not be an index. If it is ever wanted,
  it belongs behind a flag on `verify` (hash on demand, for the files whose stat already
  matched) rather than in the ingest.

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
  are empty) — check names against a dataset with real confound data when one is available. The
  same emptiness is why row *alignment* (row N ↔ volume N, ADR 0007) is asserted against the
  synthetic fixture in `test_overlay.rs::adapter_keys_confounds_rows_to_every_image_of_the_run`
  rather than against the corpus; a real confounds file would let the corpus carry it.
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

- [ ] **Rust `emit-types --from-db`**. The Python `stubgen` is the recommended path; optionally add
  a `--from-db <db>` mode to the `emit-types` bin for cargo-based workflows.

- [ ] **Consider filtering `bidslake_*` meta tables** from the generated `COLUMNS`/`C` typed surface
  (they are internal provenance tables; `bidslake_meta`/`bidslake_schema` currently appear there).

## Derivation layer

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
  `scans`, `sidecars` and the gradient tables a real `FOREIGN KEY (file_id) REFERENCES
  file_registry(file_id)`; the 22 per-row tables (`events`, `*_channels`, `motion`, `physio`,
  the adapter reader tables) reference it by convention only. The FK on the gradient payload
  caught a real dangling-reference bug immediately, which is the argument for extending it —
  but each per-row table is written by a bulk `INSERT ... SELECT` from `read_csv`, so the
  per-row constraint check has to be measured against the batched-ingest benchmark before it
  is imposed. Cheaper now than when this was written: ADR 0007 made every per-row table key
  on the file its rows came from, which is always a registry row, so there is no carve-out to
  design — expect measurement, not latent bugs of the `ds114` shape. `file_associations` is a
  separate case and cannot take one as it stands: `target_file_id` is nullable by design.

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

- [ ] **A catalog snapshot id, so a claim about it can be dated**.
  [ADR 0009](docs/adr/0009-root-tenure.md) makes an `attached` root's rows *trustworthy* — the
  indexer asserted the files would stay put, and `bidslake verify` audits that. What it cannot
  make them is **citable**: nothing records *when* the catalog last agreed with the tree, so
  "this unit is done" is a claim with no timestamp, and a consumer that wants to skip work has
  to re-check the filesystem itself. DuckLake's answer is five columns — `snapshot_id`,
  `snapshot_time`, `schema_version`, two allocator counters — plus `begin_snapshot`/`end_snapshot`
  on the file table, which turns "as of" into one predicate. For bidslake that would buy three
  things: a citable catalog version (the difference between a reproducible analysis and "as of
  whenever we ran it"), a `changes(a, b)` feed so a downstream step learns what is new rather
  than re-deriving it, and a non-destructive re-index, so the narrowing-run degradation recorded
  above becomes inspectable rather than merely regrettable. It would also let `verify` record its
  result, which is what a caller deciding on a node that never mounted the tree actually needs —
  see the `--trust-catalog` limitation in `a2cps/melodic`'s `spikes/findings.md` §16. Re-indexing
  is `DELETE` + re-insert today, so this is a real piece of work, not a column.

- [ ] **A reader for headerless numeric matrices**. FSL writes several
  (`filtered_func_data.ica/melodic_mix`, `mc/prefiltered_func_data_mcf.par`): whitespace-
  delimited, no header, and in `melodic_mix`'s case a column count that varies per run. The
  `csv` reader assumes tabs and schema-declared columns, so the `feat` adapter catalogs these
  rather than reading them. A `ContentReader` would make the mixing matrix and motion
  parameters queryable as tables instead of paths.

  Not reachable by widening the recording path, which is why this needs a reader. BIDS'
  headerless recordings are now derived rather than listed (`schema/recording.rs`: a tabular
  suffix whose column names the schema locates outside the file, in the sidecar `Columns` or
  an associated `_channels.tsv`). FSL's files are described by no BIDS rule at all — the
  `feat` term map projects `mcf.par` to `datatype=func, suffix=timeseries, desc=motion`, but
  nothing declares where its column names live, so no derivation can reach it. A reader is
  the declarative hook it needs.

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
  plus its `Examples`; the round-trip check does the rest. The artifact itself is now
  described by [ADR 0008](docs/adr/0008-layouts.md) rather than by ADR 0002 §12.

- [ ] **Enrich the under-specified `feat` roles.** Four of the 23 roles declare no `Entities`
  (`highres`, `example_func`, and the two `classification*` roles). Adding them — to the
  layout *and* the `feat` term map together, since the round trip checks one against the
  other — makes a written FEAT tree better classified when it is re-indexed. Note this does
  **not** make roles usable as source selectors, which is a structural limit rather than a
  gap ([ADR 0008](docs/adr/0008-layouts.md) §5).
