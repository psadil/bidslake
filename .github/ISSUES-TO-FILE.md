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

## Expose `timing::snapshot()` so counter assertions run under `cargo test`

What is left of "Benchmark a tree wide enough for the tabular path to show" now that
`crates/bidslake-synth` exists. The corpus half landed — `benches/ingest.rs` ingests `7t_trt` and
`synthetic`, the only vendored trees with a `scans.tsv` or a `sessions.tsv` — and the wide-header
half is `bidslake-synth`'s `ingest_wide_tabular`, at the measured 1,841 × 450 confounds shape.
What remains: `crates/bidslake/src/timing.rs` exposes `count`/`count_max`/`report` but no
snapshot, so the counter assertions worth having (one tabular group per batched read, one
undeclared statement per distinct name) still need criterion baselines rather than failing under
`cargo test`. Adding one raises its own question about the thread-locals it would read.

## One name for a motion trace, whatever produced it

`fmriprep_confounds`, `qsiprep_confounds` and `feat_motion` now declare their six rigid-body columns
from one vocabulary — the `rot_*__confounds`/`trans_*__confounds` keys in
`crates/bids-schema/data/overlays/`, byte-identical in all three, so `trans_x` is one column
definition catalog-wide — but they are still three tables. Asking for a run's motion therefore means
knowing which tool wrote it, which is the thing the shared vocabulary was supposed to stop mattering.

Merging the tables is not the answer, and it is worth writing down why so it is not re-attempted. A
per-row table carries `file_id` and nothing else structural, so concepts come from joining
`all_files`; `fmriprep_confounds` is filled by the batched `read_csv` path, which never reads
`Ingestion::materialized_concepts` (that is consumed only in DDL generation,
`crates/bidslake/src/schema/dynamic.rs`), so a merged table would leave fMRIPrep's rows with NULL
concept columns. Beyond that: `TabularRule::identity_key` cannot express "extension in
`[.tsv, .par]`" (the tabular selector parser has `intersects([suffix], …)` and no extension
equivalent); the rules disagree on `undeclared` (fMRIPrep is `catalog` for ~1,800 columns) and only
one can own the `describes` view; and grouping picks the base rule by fewest selectors, a tie that
sorts `feat.*` before `fmriprep.*` and would rename the shipped `fmriprep_confounds` table.

The cheaper shape is a view keyed on the `timeseries` suffix these tables share, unioning every
table whose `rules.tabular_data` rule selects it. `describes`
(`crates/bidslake/src/schema/ingestion.rs`) is the only view mechanism today and is per-table, so
this needs a declaration it does not have — which is the design question to settle first.

## Author layouts for fMRIPrep, MRIQC and QSIPrep

`crates/bids-schema/data/layouts/feat.json` is the only layout, so code producing files in the other
three producers' conventions hardcodes its paths. Each is one layout document plus its `Examples`,
validated against `crates/bids-schema/data/layout-metaschema.json`; the `Examples` bind the write
direction against the read side at load time, and the round-trip check does the rest. The three have
overlays under `crates/bids-schema/data/overlays/` to name their vocabulary from, and no term map yet.
ADR 0002 (`docs/adr/0002-adapters-and-layouts.md`) records the gap.

## Declare `.func.gii`, and revisit the vendored schema pin

The `bep011` overlay declares the *structural* surface family, so fMRIPrep's `.shape.gii`
morphometry and `.surf.gii` geometry now index and validate. Its surface BOLD does not:
`ds000001-fmriprep` holds 48 `_bold.func.gii`, and `.func.gii` is not in the overlay because it is
not BEP-011's — it was merged to `bids-specification` `master` separately, after the
`third_party/bids-schema` pin at 1.11.1. So the fix is a vendored-schema bump
(`tools/vendor-schema.sh`), not another overlay entry, and the bump wants doing on its own: it
moves `schema_version`, regenerates `crates/bidslake-py/python/bidslake/schema/_generated.py` under
the `codegen-drift` job, and re-runs `overlay::validate_effective`'s base/effective delta against a
render whose pre-existing metaschema deviations may differ.

Note the interaction with `crates/bids-schema/data/overlays/bep011.json` when the bump eventually
crosses BEP-011 itself: the merge is additive, so an upstream render carrying these terms verbatim
makes the overlay inert, and one carrying them with any edit makes it an `OverlayError::Conflict`
naming the pointer that drifted. Either is the signal to delete the overlay rather than reconcile
it, and `bids_schema::overlay::always_applied_overlay`'s doc comment says so.
