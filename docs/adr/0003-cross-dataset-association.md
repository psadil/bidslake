# ADR 0003 — Cross-dataset association

Status: accepted (2026-07-18)

Relates to: the static `file_associations` table (`schema.rs`) and the query layer
(`crates/bidslake-py`).

## Context

A bidslake catalog routinely holds several datasets keyed by `dataset_id`. Two of them —
`ds001761-fmriprep` and `ds001761-mriqc` — describe the *same acquisitions* under different
`dataset_id`s. A consumer (the `dirt` QC app) wants "the MRIQC `fd_mean` for this fMRIPrep BOLD
run." Today that needs a hand-written SQL join on `sub`/`ses`/`task`/`run` across `dataset_id`.

That join is **unsound**. A label like `sub-01` is only meaningful *relative to a source
dataset*; the same label is reused across unrelated datasets. Matching `sub-01` in dataset A to
`sub-01` in dataset B, with nothing tying A and B together, silently equates two possibly-different
subjects — exactly the entity string-matching the catalog exists to abolish, reintroduced across
the dataset boundary.

BIDS provides the sound anchor, and it is at the **dataset** level: `SourceDatasets` in
`dataset_description.json`. On the real data both derivatives declare the identical source DOI
`10.18112/openneuro.ds001761.v2.0.1` (fMRIPrep as a `https://doi.org/…` URL, MRIQC as the bare
DOI). That shared declaration is proof they are co-derivatives of one raw dataset — and therefore
that their `sub-01` is the same subject — *even though the raw dataset is not in the catalog*.

## Decisions

### 1. Relate datasets, not files

bidslake infers **dataset-to-dataset** relations from explicit `SourceDatasets`. It does **not**
infer file-to-file correspondence. Inferring which fMRIPrep file matches which MRIQC record by
comparing entities is the unsound step; we remove it rather than relocate it.

What a consumer does with the relation is sound *because* of it: once two datasets are known to
share a source, they share a subject/entity namespace, so matching `sub`/`ses`/`task`/`run`
*within that confirmed relation* is well-defined. bidslake supplies the relation; the caller does
the entity match (`related_datasets()` returns dataset ids, and the caller then calls
`get(dataset_id=…, sub=…, ses=…)`).

The precise file-to-file generalization — the BIDS `Sources` metadata field, a list of BIDS URIs
naming the exact input files — is deferred (see §6): no producer we have populates it (MRIQC emits
neither `Sources` nor the deprecated `RawSources`).

### 2. Store *declarations*; resolve the relation in a query-time view

Two static tables, each keyed by a single `dataset_id` (so the crate invariant "every table is
keyed by `dataset_id`" holds and each ingest writes only its own rows):

- **`dataset_links`** — what a dataset *declares it came from*: one row per `SourceDatasets` entry
  (`link_type='source'`), per `--source-dataset` flag (`'declared'`), and per `DatasetLinks`
  mapping (`'named'`). Carries the verbatim `declared_ref` and the canonicalized
  `identity`/`identity_kind`/`identity_base`.
- **`dataset_identity`** — what a dataset *is*: its own `dataset:<id>`, its `DatasetDOI`, and its
  `root_uri` (`source` records which).

The dataset-to-dataset relation is **not stored**. It is the `dataset_relations` **view**:

- `shares_source` — two datasets declare the same source identity (co-derivatives). Needs no
  `dataset_identity` row, which is why it resolves when the shared source is absent from the
  catalog (the motivating case).
- `derived_from` / `source_of` — one dataset declares an identity that another *present* dataset
  *is*.

Depth-1 only: `UNION` dedups, `from <> to` drops self-links, so cycles cannot arise. Transitive
derivative-of-derivative chains have no caller and are out of scope.

**Why no stored `target_dataset_id`.** A resolved target is a cache whose correctness depends on
what else is in the catalog — and the catalog grows. Ingest MRIQC (its source absent → the target
would be NULL), then later ingest the raw dataset, and every stored NULL is now wrong until MRIQC
is re-indexed. That *is* the ingest-ordering problem, moved into a column. Resolving in a view over
a tens-of-rows table costs nothing on read and makes ingest order irrelevant: `A`-then-`B` and
`B`-then-`A` produce byte-identical catalogs, and a source added later just makes the edge appear
on the next query — no bookkeeping, no re-index. This mirrors the additive, `WHERE NOT EXISTS`
property the rest of the schema already guarantees, and is validated by `ingest_order_does_not_matter`.

### 3. One identity normalization, and it is where the feature lives

`links::canonicalize` maps any declared reference — a bare DOI, a `https://doi.org/…` URL, a
repository URL, a filesystem/S3 path, or a `dataset:<id>` — to a stable identity. The single rule
that makes the whole feature work: **DOIs are lowercased** (case-insensitive per the Handle spec)
after their resolver prefix is stripped, so MRIQC's bare `10.18112/…` and fMRIPrep's
`https://doi.org/10.18112/…` collide. Unrecognizable references become `opaque:<verbatim>` rather
than errors — the same best-effort, keep-everything contract as `file_associations` (identical
garbage still collides, which is what a user who typed the same thing twice meant).

Versions are part of the identity: `…v2.0.1` and `…v2.0.0` do **not** auto-link (subjects are
added/removed between OpenNeuro versions; a cross-version link is a guess dressed as a proof). The
version-stripped `identity_base` is stored only so `bidslake link list` can *warn* about drift; the
escape hatch (§4) forces a cross-version link when the user knows better.

### 4. An escape hatch for datasets with no DOI

Not every dataset has a `SourceDatasets` DOI. `--source-dataset <ref>` on `index` (repeatable)
declares a source through the *same* `canonicalize`, as a `declared` link:
`--source-dataset ds001761-fmriprep` (a `dataset:` identity → a `derived_from` edge against the
target's `self`), or `--source-dataset <doi>` (a `shares_source` edge). `bidslake link add/rm`
do the same post-hoc, and `link init` creates the tables/view *and backfills declarations from the
stored `dataset_description` rows*, so a catalog indexed before this feature gains links with no
re-index. `declared` links are the user's and are never cleared by a re-ingest; the `source`/`named`
links (derived from `dataset_description.json`) are refreshed each ingest so they track the file.

### 5. Two datasets are co-derivatives if they share *any* source

Pipelines list templates, atlases, and code URLs in `SourceDatasets` alongside the real source, so
requiring identical source *sets* would fail on real data. Sharing **any** source identity suffices.
This has an honest soundness gap — a dataset derived from A+B relates to one derived from A alone —
but it is bounded by the consumer's suffix + entity match, and `dataset_relations.via_identity`
records *which* source justified each edge, so it is auditable rather than silent.

### 6. File-level provenance (`Sources`) is deferred, not designed away

The precise mechanism is the BIDS `Sources` metadata field: a list of BIDS URIs
(`bids:<name>:<path>`) naming the exact input files, resolved through `DatasetLinks`. It would let
`file_associations` generalize across datasets (a `target_dataset_id`, widening the PK, and fixing
`_associated_for`'s dataset_id/root_uri stamping — a coordinated ~6-site change). It is deferred
because no producer we have emits `Sources` (MRIQC emits nothing; fMRIPrep only the deprecated
`RawSources`), so it cannot be exercised. We target `Sources` (spec-current, not the ambiguous
bare-path `RawSources`) and have filed an upstream request to MRIQC.

As the first, safe step of that machinery, `normalize_path` (the `IntendedFor` resolver) was fixed
now: it split only `bids::` and turned a named `bids:deriv:sub-01/x` into the garbage path
`sub-01/bids:deriv:sub-01/x`. It now parses `bids:<name>:<path>` and *skips* (rather than
fabricates) a target that names another dataset.

## Consequences

- **Ingest order is irrelevant, and there is full S3 parity.** Everything is read from
  `dataset_description.json` (read on S3) and resolved by a view over tables populated on S3 —
  unlike `resolve_structural_associations`, which needs the in-memory `FileTree` and is local-only.
- **`dataset_id` is a free-text, mutable `Name`.** The `shares_source` path keys on the DOI, not
  `dataset_id`, so it survives a rename; only `--source-dataset <id>` is fragile (documented; prefer
  a DOI). `Name`-collision-merges-datasets is a pre-existing hazard this feature does not fix.
- **Public API.** Rust: `bidslake link init|add|list|rm`, `--source-dataset`. Python:
  `lake.datasets()`, `lake.dataset_relations()`, `lake.related_datasets(id, relation=…)`,
  `BidsFile.related_datasets(…)`, and the `Relation` enum (`shares_source`/`derived_from`/`source_of`
  — deliberately not DataLad's "sibling", which means a clone of the *same* dataset).
