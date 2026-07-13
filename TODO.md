# Follow-ups

Deferred / optional items surfaced by the July 2026 design sweep but left out of the
remediation pass (see the finding ids in parentheses). Recorded here for later; not filed
as issues. Roughly ordered by value.

- [ ] **Genuine lazy `get()` streaming** (`py-04`). `get()` is typed `Iterator[BidsFile]` but
  materializes the whole Arrow-IPC buffer + Polars frame first, so its laziness is cosmetic
  (now documented in the docstring). When the PyO3 PyCapsule stream bridge lands
  (`crates/bidslake-py/src/lib.rs`), stream Arrow batches so `get()` is O(1) memory.

- [ ] **Cache parsed selector expressions** (`idiom-04` note). Each rule's `selectors_raw` is
  re-`replace`d, re-allocated, and re-parsed by oxc on every `Tabular::route`
  (`crates/bids-schema/src/expression.rs`). Caching the parsed AST per rule is the real ingest
  perf win; the loop-hoist already applied does not address it.

- [ ] **Eliminate the per-file entity-map rebuild** (`dup-02` follow-up). `entity_name_to_key`
  is now shared, but `build_file_context` (`crates/bids-schema/src/context.rs`) still rebuilds
  the map on every file, and the two per-file loops (`crates/bids-validator-rs/src/context.rs`,
  `crates/bidslake/src/bids.rs`) trigger it. Hoist/memoize so it is computed once per schema.

- [ ] **Push `materialize` scheme-branching behind the trait** (`abstraction-01` secondary).
  Have each `BidsFileSystem` impl return a `read_csv`-ready source string (absolute local path,
  or `s3://` URL) so the `starts_with("s3://")` branch at the call sites can be deleted;
  optional rename `materialize` → `read_csv_source`.

- [ ] **Fully convert `db.rs`/`dynamic.rs` to `anyhow`** (`eh-05` optional). Beyond the
  call-site `.context()` already added, push table/path context inside the write layer. Requires
  rewriting the two manual `duckdb::Error::ToSqlConversionFailure` constructions in
  `crates/bidslake/src/schema/dynamic.rs`.

- [ ] **Implicit-insert vs `participants.tsv` ordering** (`eh-04` note). Under the
  `WHERE NOT EXISTS` guard, whichever of the implicit participant/session insert vs the
  `participants.tsv`/`sessions.tsv` ingest runs first wins — so a bare implicit row can shadow the
  richer tabular row. A real, distinct correctness concern worth its own investigation.

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

- [ ] **Flesh out the bundled MRIQC/QSIPrep overlays**. `crates/bids-schema/data/overlays/{mriqc,qsiprep}.json`
  are valid-but-empty stubs; author them (IQMs, group TSVs, transforms, pipeline entities/fields)
  from the OpenNeuroDerivatives corpus, like the fMRIPrep overlay.

- [ ] **YAML overlay authoring**. Overlays are JSON-only; accept `.yaml`/`.yml` (parse to `Value`
  before merge) behind an optional `yaml` cargo feature.

- [ ] **Opt-in stubgen for augmented types**. Generate a project-local typed module (Literals +
  extended `GetFilters` + `C`) from an augmented DB's stored effective schema — a `--from-db` mode
  on `emit-types` and/or `python -m bidslake.stubgen`. While there, filter the `bidslake_*` meta
  tables out of the generated `COLUMNS`/`C` (now that `bidslake_schema` is always stamped).

- [ ] **Python runtime ergonomics**. Dynamic `C`-style column accessors from live introspection so
  augmented tables get them without codegen; make `_warn_on_version_mismatch` read
  `effective_schema()`/`overlays()` and nudge toward stubgen when a DB is augmented.

- [ ] **`bidslake schema --diff` / `index --dry-run`**. Experimentation surfaces: print the DDL/table
  delta an overlay would produce (no DB written), and a per-file routing preview (which table each
  file hits, what stays `skipped`). Pairs with `bids-validator --overlay` for the file-recognition half.

- [ ] **Dataset-embedded overlay auto-discovery**. Auto-apply a `.bidslake/overlay.json` at the
  dataset root (lowest precedence) so a derivative dataset self-describes with zero flags; reuses
  `overlay::{load_overlay, merge_into}`.

- [ ] **Schema-driven ordering, once BIDS has `row_order`**. `bids::is_order_insensitive` hardcodes
  the row-order policy (only `events` is reorderable). If bids-standard/bids-2-devel#98 lands, drive
  it from the schema and drop the hardcode. Related: declarable `row_identity` (blocked on the same
  "no invented schema concepts" constraint).
