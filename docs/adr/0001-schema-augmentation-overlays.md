# ADR 0001 — Schema augmentation via additive overlays

Status: accepted (2026-07-13)

## Context

bidslake derives its entire DuckDB schema at *runtime* from the vendored BIDS
`schema.json`. This keeps the catalog faithful to the formal standard, but the
standard evolves slowly: flagship BIDS apps (fMRIPrep, MRIQC, QSIPrep) emit
"bidsish" derivative files that are not in the schema and pass validation only by
hiding behind a `.bidsignore`. Users could not index most of what they wanted from
these pipelines — derivative tabular files (e.g. `*_desc-confounds_timeseries.tsv`)
were routed nowhere and recorded `skipped`; non-BIDS entities (fMRIPrep's
`from`/`to`/`mode` on transforms) produced no queryable columns.

We needed a way for users to teach bidslake about these outputs that (a) works for
both DB construction and Python query, (b) does not fork or drift from the vendored
BIDS schema, and (c) does not silently invent BIDS semantics.

## Decisions

### 1. Overlays, not schema replacement

Users supply a metaschema-conformant **overlay** — a partial BIDS schema (same
`objects.*` / `rules.*` shape) — that is deep-merged onto the base schema before
generation. Every DDL/ingest generator (and the validator's `BidsSchema::from_value`)
already reads the schema as a `serde_json::Value`, so a merged fragment lights up new
tables, columns, and generated entity columns through the existing code with no
per-generator change.

*Rejected:* full-replacement schema (today's `--schema-path`) forces users to fork
and maintain the whole schema and drift on every BIDS bump; a bespoke mini-format
would need a second parser and evaluator.

Implementation: `bids_schema::overlay` (shared, since both `bidslake` and the
validator depend on it). `Schema::load_with_overlays` in `bidslake`.

### 2. Additive-only merge

An overlay may add object keys and extend arrays, but never rewrite or delete a value
the base already defines; a conflicting scalar is an error naming the JSON pointer.
This makes merging order-independent and prevents a typo from shadowing BIDS
semantics. See `overlay::merge_into`.

### 3. Metaschema validation by *delta*

Overlays are validated against the BIDS metaschema — but the vendored base schema
itself does not fully satisfy the vendored metaschema (an inherent upstream lag, e.g.
`rules.dataset_metadata` is unknown to the metaschema). So validating the merged
schema outright would reject even a no-op overlay. Instead `overlay::validate_effective`
computes the *delta*: it fails only on metaschema violations the overlay *introduces*,
tolerating pre-existing base deviations. Uses the `jsonschema` crate.

### 4. Self-describing databases

Every catalog embeds its effective schema in `bidslake_schema`; when overlays were
applied, their provenance (source, sha256, content) is recorded in
`bidslake_overlays`. The augmentation travels with the data — the Python query side
and codegen recover it without re-passing anything (the Iceberg/Delta principle). See
`db::BidsDb::stamp_schema`.

### 5. Vendoring the schema and metaschema from two pinned sources

The compiled schema and the metaschema live in **different** upstream repositories:
`schema.json` in `bids-standard/bids-schema`, `metaschema.json` in
`bids-standard/bids-specification` (the `bids-schema` repo has no metaschema). A full
git subtree of either is heavy (~92 MB for bids-schema, mostly PR/BEP renders). So we
vendor both as lean, pinned, in-tree files (`third_party/bids-schema/...` and
`third_party/bids-specification/...`, each with a `.pinned-commit`), refreshable via
`tools/vendor-schema.sh`, embedded at build time (`bids_schema::{SCHEMA_JSON,
METASCHEMA_JSON}`). Builds stay fully offline.

### 6. Row-order stays upstream; positional handling stays hardcoded

> **Superseded by [ADR 0002](0002-layout-adapters.md) §4 (2026-07-14).** The conclusion below
> — that the only alternatives were "invent a BIDS concept" or "hardcode it" — was a false
> dichotomy. Read-vs-catalog and row-order are not BIDS questions at all (BIDS has no
> database), so they needed no *BIDS* concept; they needed a **bidslake** schema. They now
> live in the formal, metaschema-validated **ingestion schema**
> (`data/ingestion/base.json` declares `events` as `{"ordered": false}`), and
> `bids::is_order_insensitive` no longer exists. The reasoning about not inventing *BIDS*
> concepts still stands, and bids-2-devel#98 remains the upstream home for row-order if BIDS
> ever wants it — but bidslake no longer waits on it. Retained below as written.

Some tabular files raised a real gap: `*_desc-confounds_timeseries.tsv` rows are
positional (row N == volume N), so their order must be preserved. BIDS has no schema
field to express row-order semantics. We deliberately do **not** invent one, nor
extend the metaschema in-memory to allow it — that deviation would cause long-term
headaches. Instead the ordering policy stays **hardcoded** in `bids::is_order_insensitive`
(only `events` is reorderable; everything else, including positional derivative time
series and recordings, preserves TSV line order so `row_idx` is a faithful row
number). A `row_order` schema field is proposed upstream at
[bids-standard/bids-2-devel#98](https://github.com/bids-standard/bids-2-devel/issues/98);
if adopted, the hardcode can be driven from the schema.

The same reasoning defers the other "lift the hardcoded limits" items (declarable
`row_identity`, etc.) wherever they would require inventing schema concepts.

### 7. Overlays need to walk past `.bidsignore`

Pipelines hide their non-standard outputs from BIDS validation with a `.bidsignore`
(fMRIPrep lists `*_timeseries.tsv`, `*_xfm.*`, `*_boldref.nii.gz`, …) — i.e. it hides
*exactly* the files an overlay exists to index. bidslake's walker honors `.bidsignore`,
so by default an overlay is inert against a real derivative dataset. The `index
--no-bidsignore` flag walks every file so overlay-described outputs are indexed;
bidslake's own classification still decides what becomes a scan/table, so reports/logs
(`*.html`, `figures/`) are walked but not indexed. Verified against a real fMRIPrep
dataset (`third_party/bids-examples/ds000001-fmriprep`): without the flag its transforms
and confounds are invisible; with it, 48 transforms parse with `from`/`to`/`mode` and the
confounds route to `fmriprep_confounds`.

Kept as an explicit flag rather than implied by `--overlay` (auto-relaxing `.bidsignore`
whenever an overlay is present is a reasonable future default; see the TODO).

## Consequences

- New derivative outputs become first-class (real tables, generated entity columns,
  typed sidecar fields) via a small additive overlay, and the augmentation is
  reproducible from the database itself.
- Static Python typing of augmented columns requires regenerating types per project
  (opt-in stubgen); runtime querying works with no extra step, because column
  validation is against the live `information_schema`.
- bidslake honors only what the metaschema (as vendored) permits; it will not accept
  overlays that invent new schema constructs until those land upstream.
- ~~The ordering hardcode is a known, documented exception to bidslake's
  "everything is schema-driven" design, tracked against bids-2-devel#98.~~ Resolved: the
  exception is gone. The ordering policy (and read-vs-catalog generally) is now driven by the
  **ingestion schema** — see [ADR 0002](0002-layout-adapters.md) §4.
