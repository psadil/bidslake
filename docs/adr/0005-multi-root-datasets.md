# ADR 0005 — A dataset may span several ingest roots

```
ADR: 0005
Title: A dataset may span several ingest roots
Status: Provisional
Type: Design
Created: 12-Aug-2026
```

## Abstract

A dataset's ingest roots live in their own table, `dataset_roots`, keyed `(dataset_id, root_uri)`,
and `dataset_description` carries no root. A second root under an existing `dataset_id` is
additive; where that id was *inferred*, four `dataset_description.json` fields decide whose it is.

## Motivation

`dataset_id` cannot both name the logical dataset and the tree its files resolve through. A stored
row reaches an openable file only through `root_uri` plus a root-relative `file_path`, so a root on
the one-per-dataset `dataset_description` row admits one tree; a second has nowhere to go.

**Subject-sharded pipeline output is one logical dataset with one root per subject** — the normal
way fMRIPrep and FreeSurfer are run at scale. One `dataset_id` per shard repeats everything
dataset-scoped: `dataset_description`, `dataset_links`, `dataset_identity`, and `participants`,
which stops being a list of participants once the same person is listed once per shard.
`shares_source` fires *between shards of one run*, producing N×(N−1) edges of a dataset with
itself, burying the fMRIPrep↔MRIQC relation [ADR 0003](0003-associations.md) exists to surface.

A rule admitting second roots cannot key on `Name`. `Name` is the *tool's* name for every derivative
a pipeline writes — every fMRIPrep output declares `"fMRIPrep - fMRI PREProcessing workflow"` — and
hand-written datasets collide too: of the 108 top-level `dataset_description.json` files in
`third_party/bids-examples`, four pairs of distinct datasets share a `Name`. `Name` is also what an
id is *inferred* from, so the inferred case is the whole of the ambiguity.

A `dataset_description.json` is re-read every run, so first-writer-wins keeps a stale row: a
description added or corrected since the first index never reaches the catalog.

## Rationale

### On a URI as the root's key

DuckDB dictionary-encodes the column, so repeating one per row costs a code, not a path. Holding
every shard under one `dataset_id` is what pays: `dataset_relations`'s `from <> to` drops the
self-pairs, and `participants` dedupes on its `(dataset_id, participant_id)` primary key.

### On the four identity fields

`SourceDatasets` carries the weight — it is where two studies' fMRIPrep output differ; `GeneratedBy`
refuses too, since shards processed months apart under different pipeline versions are not
self-evidently one dataset. `Authors` or `HowToAcknowledge` drifting is untidy, not evidence, so the
set stops there. Shards of one run carry byte-identical descriptions, so no flags are needed.

### On the limit of inference

The basename fallback is a real constraint, not an oversight. `SUBJECTS_DIR`s sharing a name
(`work/sub-01/freesurfer`, `work/sub-02/freesurfer`) merge with no flags, with no descriptions to
disagree; roots named for their subject (`fs/sub-01`, `fs/sub-02`) infer different ids and stay
separate, correctly: nothing distinguishes them from unrelated trees. `--dataset-id` joins those.

### On re-recording every root

`clear_derived_links` drops a dataset's `dataset_identity` rows before `record_links` writes, so
recording only this run's root would leave a re-indexed shard blind to the roots it did not walk.

### On replacing the description row

Because the write replaces rather than defers, only the shallowest description is written: a nested
one under `derivatives/` would otherwise overwrite the parent's row. The synthesized row exists
because `lake.datasets()` enumerates this table and the `files` view sources `dataset__*` from it.

## Specification

### 1. `dataset_roots` is a dataset's registry of ingest roots

```sql
CREATE TABLE IF NOT EXISTS dataset_roots (dataset_id TEXT, root_uri TEXT,
    tenure TEXT NOT NULL DEFAULT 'attached' CHECK (tenure IN ('attached','managed')),  -- ADR 0007
    PRIMARY KEY (dataset_id, root_uri));
```

`tenure` is [ADR 0007](0007-root-tenure.md)'s, and `dataset_description` has no `root_uri` column.

### 2. A root is identified by its URI

There is no short per-root label and no flag to assign one.

### 3. A file is the triple `(dataset_id, root_uri, file_path)`

`file_path` is relative to the root it was walked from, so `root_uri` + `file_path` reopens it; two
roots may hold the same path, and `file_id` hashes the triple ([ADR 0006](0006-file-registry.md)).

### 4. Everything dataset-scoped is keyed by a single `dataset_id`

Roots are additional, never a replacement.

### 5. Registering a root runs after the `dataset_id` is resolved, and is additive

`BidsParser::resolve_root` binds this run's root to the resolved id:

- **Already registered** → identity is not in question; re-register, which raises tenure to
  `managed` if this run asserts it and never lowers it.
- **`--dataset-id` asserted**, dataset present under a different root → register; if the incoming
  `dataset_description.json` differs from the stored row, **warn**, naming the fields that disagree.
- **Id inferred**, dataset present under a different root → compare incoming against stored on
  **`Name`, `BIDSVersion`, `GeneratedBy`, `SourceDatasets`**. All four equal → merge silently.
  Otherwise **refuse**, listing the existing roots and both ways out: `--dataset-id <this-id>` to
  join it, `--dataset-id <other>` to keep it separate.

No `dataset_description.json` → the id is the root basename, or an `s3://` prefix's last component.

### 6. Every root of a dataset is an identity the dataset *is*

`record_links` re-records a `root_uri` identity for **every** row in `dataset_roots`, not only the
root this run walked — which is what a relative `DatasetLinks` value resolves against. `link init`'s
backfill, having no run, resolves against the lexicographically first `root_uri`: a relative link
describes the tree it was written in, and every root of one dataset holds that tree.

### 7. A description refreshes on re-index; a synthesized row never shadows it

A real `dataset_description.json` uses `Schema::insert_or_replace` (`INSERT OR REPLACE`); only the
**shallowest** in a tree is written. A dataset with none gets a row of only `dataset_id`, carrying
the `WHERE NOT EXISTS` guard so it never displaces a real one.

## Backwards Compatibility

A catalog built before `dataset_roots` existed has no such table, and a `root_uri` column on
`dataset_description` nothing reads. `create_tables` adds the table on the next `index` without
backfilling it, and the Python read path queries it directly, so such a catalog must be re-indexed.
For a single-root dataset, root-relative and dataset-relative `file_path`s coincide.

## Rejected Ideas

**A `root_id` derived from the root's final path component, with a `--root-id` override.** It buys
an escape hatch for a default that would not otherwise need one — two roots whose directories share
a basename, `scratch/sub-01/fmriprep` and `scratch/sub-02/fmriprep` — and adds a stored key that
`file_path` resolves through and that must therefore never change. A URI needs neither.

**Comparing the two descriptions as text.** The stored side is compact JSON in a `VARCHAR` column,
so a `SourceDatasets` written with its keys in a different order would count as a mismatch and
refuse a valid merge; `description_mismatches` parses both sides back to `serde_json::Value` first.

**Deriving the root set with `SELECT DISTINCT root_uri FROM file_registry`.** Silent about a root
that contributed no rows — an ingest that found nothing, or whose every file an `ignore` rule
claimed — which is the root whose status a caller cannot otherwise establish.

**Keeping `root_uri` on `dataset_description` too.** One column holds one root, so it is a second,
always-partial answer to "where does this dataset live" — the ambiguity the registry removes.

**Refusing a second root under an asserted `--dataset-id` whose description disagrees.** A user
naming the dataset makes a claim the catalog has no standing to overrule, so a mistyped flag merges
two datasets, and the disagreement is warned about rather than enforced.

## Open Issues

- No verb removes a root from a dataset or splits a merged one. A root registered under the wrong
  `dataset_id` is separated only by rebuilding the catalog.
