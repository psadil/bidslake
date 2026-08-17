# ADR 0001 — Schema augmentation via additive overlays

```
ADR: 0001
Title: Schema augmentation via additive overlays
Status: Provisional
Type: Design
Created: 13-Jul-2026
```

## Abstract

bidslake derives its whole DuckDB schema at runtime from the vendored BIDS `schema.json`. An
**overlay** is a partial BIDS schema deep-merged onto that base before generation, turning
vocabulary the standard has not reached into real tables and columns: additive-only,
metaschema-checked by delta, applied as an unordered set, stamped into the catalog.

## Motivation

The standard evolves slowly and the flagship BIDS apps do not wait: fMRIPrep, MRIQC and QSIPrep emit
"bidsish" derivatives absent from the schema, hidden behind a `.bidsignore` to pass validation.
bidslake then indexes almost nothing a user wants: `*_desc-confounds_timeseries.tsv` routes nowhere
and is recorded `skipped`, and a transform's `from`/`to`/`mode` give no queryable columns, for DDL
or Python query alike, no BIDS entities of those names existing. `--schema-path`, the only lever,
replaces the schema wholesale. Vocabulary alone would not reach them either: the walker honors
`.bidsignore`, and fMRIPrep's lists `*_timeseries.tsv`, `*_xfm.*`, `*_boldref.nii.gz` — exactly what
an overlay exists to index.

## Rationale

Every consumer reads the schema as a `serde_json::Value` — the DuckDB table generators, the
validator's `BidsSchema::from_value` — so a merged fragment lights up new tables, columns and
generated entity columns with no per-consumer change. Stamped into the catalog, it reaches past
the process: Python query and `--from-db` stubgen recover a catalog's schema without re-passing it.

Additive-only merging keeps a typo from shadowing BIDS semantics — an overlay disagreeing with the
base fails at the JSON pointer rather than redeclaring `trans_x` as a string — and buys
order-independence for objects. Arrays do not: merging appends the overlay's new elements after the
base's, so folding over `[a, b]` and `[b, a]` differ in element order whenever two overlays extend
one array. Sorting the fold by serialized content, not by the caller's names, is what makes two
callers naming one set differently get one schema.

Validation is by delta because schema and metaschema come from separate upstreams and the metaschema
lags: the vendored BIDS 1.11.1 render carries `rules.dataset_metadata`, absent from the metaschema's
`rules`, so validating a merged schema outright would reject even a no-op overlay. Reporting only
error signatures — instance pointer plus message — absent from validating the base alone catches an
overlay-added bad key where a base-added one beside it is tolerated.

`--no-bidsignore` is a separate flag because relaxing a dataset-wide walk guard is not something a
vocabulary flag should change silently. On `third_party/bids-examples/ds000001-fmriprep`, without it
the tree's 48 `*_xfm.*` transforms and 12 `*_timeseries.tsv` confounds files are invisible; with it,
the transforms parse with `from`/`to`/`mode` and the confounds route to `fmriprep_confounds`.

## Specification

### 1. An overlay is a partial BIDS schema merged onto the base before generation

An overlay is a JSON object with the same `objects.*`/`rules.*` shape as `schema.json`, deep-merged
into the schema `Value` by `Schema::load_full` before any DDL is generated. Overlays arrive by path
(`--overlay`), from a dataset's `.bidslake/overlay.json`, or bundled under `--adapter <name>` beside
that producer's term map and ingestion fragment ([ADR 0002](0002-adapters-and-layouts.md)).

### 2. Merging is additive-only, atomic, and applied as a set

- **object ⊕ object**: recurse key by key; a key only the overlay has is inserted.
- **array ⊕ array**: append each overlay element not already present, by structural equality.
- **anything else**: equal values are a no-op, so re-applying an overlay is idempotent; a differing
  value, kind mismatches included, is an error naming the RFC 6901 pointer.

An overlay may add and extend, never rewrite or delete; a failed merge names the offender and leaves
the base as found. `merge_all` sorts a set by serialized content when folding, so the effective
schema depends on *which* overlays were named, not their order, each overlay's array order surviving
as written.

### 3. An overlay is validated against the BIDS metaschema by delta

`validate_effective(pre_overlay, effective)` runs the vendored metaschema over both documents with
the `jsonschema` crate and fails only on violations the overlay introduces, tolerating those the
base already carries. An overlay may use only constructs the vendored metaschema already permits.

### 4. Every catalog embeds the schema it was built from

`stamp_schema` writes one `bidslake_schema` row — `base_schema_version`, the full `effective_schema`
JSON, an `overlay_digest` over the applied overlays (NULL when none) — and, where overlays applied,
`bidslake_overlays`, one row each with `idx`, `source`, `sha256` and verbatim `content`.

### 5. The schema and the metaschema are vendored from two pinned sources

`schema.json` comes from `bids-standard/bids-schema`, `metaschema.json` from
`bids-standard/bids-specification`, which ships the metaschema the schema repo lacks. Each is pinned
under `third_party/` as a lean file rather than a git subtree, beside a `.pinned-commit`, refreshed
by `tools/vendor-schema.sh` and embedded at build time.

### 6. Indexing overlay-described output requires `--no-bidsignore`

`index --no-bidsignore` walks every file regardless of the dataset's `.bidsignore`, nested ones
included; it is explicit, not implied by `--overlay` or `--adapter`. Walking is not indexing:
classification still decides what becomes a scan or a table, so `*.html` is walked, not indexed.

## Backwards Compatibility

A catalog built without overlays is unaffected. Tables are created `IF NOT EXISTS`, so an
overlay-added table appears on the first run carrying it, while the columns of a table already
present are frozen by the run that created it: widening an overlay over an existing table needs a
fresh catalog ([ADR 0006](0006-file-registry.md)). Statically typed overlay columns must be
regenerated by stubgen from the stored `effective_schema`; runtime query needs no regeneration,
validating columns against the live `information_schema`.

## Rejected Ideas

**A full-replacement schema.** `--schema-path` does this and is the wrong tool: the user forks all
of `schema.json` and re-merges on every BIDS release, where an overlay carries only the delta.

**A bespoke augmentation mini-format.** It would need its own parser and evaluator; an overlay
reuses the metaschema, the merge, and every generator that already reads the schema `Value`.

**Canonicalizing each array's appended tail.** Deterministic and wrong: an overlay's array order is
intent. `rules.entities` is the BIDS entity ordering, so reordering `from`, `to`, `mode` makes
`from-X_to-Y_mode-Z` fail entity-order validation; the test
`one_overlays_own_array_order_is_preserved` pins it.

**A BIDS schema field for row-order.** Rows of `*_desc-confounds_timeseries.tsv` are positional and
BIDS has no way to say so, but a field declaring it makes bidslake the de facto author of BIDS
semantics; `row_order` is proposed upstream at
[bids-2-devel#98](https://github.com/bids-standard/bids-2-devel/issues/98) and bidslake does not
front-run it. Such a concern gets a bidslake schema of its own
([ADR 0002](0002-adapters-and-layouts.md)), and the same refusal defers a declarable `row_identity`,
still a hardcoded match on table name in `crates/bidslake/src/schema/tabular.rs`.

**Loosening the metaschema in memory.** The same authorship problem from the other side: it would
let an overlay declare constructs BIDS has not adopted.

## Open Issues

- **Should an adapter relax `.bidsignore` by itself?** Implying `--no-bidsignore`, or un-ignoring
  only what the effective schema recognizes, would make the common case one flag: MRIQC's metrics
  live in `*_T1w.json`/`*_bold.json` sidecars its `.bidsignore` hides, so `--adapter mriqc` reaches
  none.
