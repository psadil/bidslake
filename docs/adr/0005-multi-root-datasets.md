# ADR 0005 — A dataset may span several ingest roots

Status: accepted (2026-08-12)

Relates to: `dataset_roots` and the `dataset_description` contract (`schema.rs`),
`BidsParser::resolve_root` (`bids.rs`), and root resolution in `bidslake-py` (`paths.py`,
`layout.py`). Supersedes ADR 0002 §3's "a dataset also has exactly one root" and §10's
placement of `root_uri` on `dataset_description`.

## Context

`dataset_id` was doing two jobs at once: naming the logical dataset, and naming the ingest
root. The second job was structural — `root_uri` lived on the `dataset_description` row, one per
dataset, and `root_uri` + a dataset-relative `file_path` is the only route from a stored row
back to an openable file. So a second root under one `dataset_id` was not additive, and
`check_dataset_root` refused it.

The refusal was not the bug. **Subject-sharded pipeline output is one logical dataset with one
root per subject** — the normal way fMRIPrep and FreeSurfer are run at scale — so the model
forced it apart into one `dataset_id` per shard. Everything dataset-scoped then repeated per
shard: `dataset_description`, `dataset_links`, `dataset_identity`, and `participants` (the same
person once per shard, so `participants` stopped being a list of participants). `shares_source`
fired *between shards of the same pipeline*, producing N×(N−1) edges of a dataset with itself
and burying the real fMRIPrep↔MRIQC relation ADR 0003 exists to surface. None of it was
recoverable downstream: nothing recorded that two shards were one dataset — that lived only in
the id strings the user chose.

Two further problems surfaced while fixing it, neither in the original write-up:

- **The guard was aimed backwards.** `check_dataset_root` early-returned when `dataset_id` was
  `None`, and ran *before* the id was inferred from `Name`. So it fired only when the id was
  **asserted** — the unambiguous case — and never when it was **inferred**. Since every fMRIPrep
  output declares the identical `Name: "fMRIPrep - fMRI PREProcessing workflow"`, indexing shards
  with no `--dataset-id` inferred one id for all of them and produced exactly the silent
  misresolution the guard was written to prevent.
- **`Name` is not an identity.** It is the *tool's* name for every derivative a pipeline writes.
  Even in the small vendored corpus, four pairs of unrelated datasets share a `Name`
  (`micr_SEM`, the HED face-processing pair, the Brainstorm epileptogenicity pair, the qMRI VFA
  pair).

## Decisions

### 1. `dataset_roots` is the registry; `dataset_description.root_uri` is gone

```sql
CREATE TABLE dataset_roots (dataset_id TEXT, root_uri TEXT, PRIMARY KEY (dataset_id, root_uri));
```

One row for an ordinary dataset, N for a sharded one. Removing `root_uri` from
`dataset_description` is the point rather than tidying: leaving it would keep a second, always
partial answer to "where does this dataset live", which is the column the problem was made of.

The registry is explicit rather than `SELECT DISTINCT root_uri FROM …` because it is
authoritative for a root that contributed no rows at all — an ingest that found only tabular
files, or none.

### 2. The root's URI is its own identifier

No short per-root label. A label needs a derivation rule, an escape hatch for when that rule
collides (two roots whose directories share a basename, `scratch/sub-01/fmriprep` and
`scratch/sub-02/fmriprep`), and a guarantee it never moves, since it would be a stored key that
`file_path` resolves through. A URI needs none of that, and DuckDB dictionary-encodes the
column so repeating it costs a code rather than a path.

*Rejected:* a `root_id` derived from the root's final path component plus a `--root-id`
override. It was carried through two drafts before anyone asked what it bought; the answer was
"an escape hatch for a default we chose", which is not a reason.

### 3. A second root just works — and the identity rule decides when the id was inferred

`check_dataset_root` is deleted. `resolve_root` runs **after** the `dataset_id` is resolved,
which is what closes the backwards-guard hole above, and applies:

- **`--dataset-id` asserted** → the user stated the identity. Register the root. If the incoming
  `dataset_description.json` differs from the stored row, **warn** — under an asserted id a
  differing `Name` is the strongest available signal that the flag was mistyped.
- **id inferred**, and already in the catalog under a different root → compare the incoming
  description to the stored row on **`Name`, `BIDSVersion`, `GeneratedBy`, `SourceDatasets`**.
  All four equal → merge silently. Otherwise **refuse**, naming both resolutions.

Shards of one pipeline run carry byte-identical descriptions, so the sharded workflow needs no
flags at all. Two studies' fMRIPrep output differ in `SourceDatasets` — which is exactly what
`Name` cannot distinguish — and are caught.

Comparison is on parsed `serde_json::Value`s, not text, so two descriptions that differ only in
key order still merge. The stored side is compact JSON in a `VARCHAR` column, so a textual
comparison would refuse valid merges.

**The limitation, stated plainly.** Inference keys on the *root directory's name* when there is
no `dataset_description.json`. Per-subject `SUBJECTS_DIR`s that share a name
(`work/sub-01/freesurfer`, `work/sub-02/freesurfer`) merge with no flags, because neither has a
description and there is nothing to disagree about. Roots named after their subject
(`fs/sub-01`, `fs/sub-02`) infer different ids and stay separate — correctly, since nothing
distinguishes them from two unrelated trees. `--dataset-id` is what joins those.

### 4. Every root is an identity the dataset *is*

`record_links` re-records a `root_uri` identity for **every** row in `dataset_roots`, not just
the root this run walked. `clear_derived_links` drops all of a dataset's `dataset_identity` rows
before re-recording, so recording only `self.fs.root()` would make re-indexing shard B silently
forget shard A's root.

### 5. A `dataset_description.json` now refreshes on re-index (closes `eh-04`)

A dataset's description is re-read every run, so first-writer-wins meant a re-index kept a stale
row — a description added or corrected since the first index never reached the catalog. A real
description now upserts (`Schema::insert_or_replace`, a `guard: bool` on the existing
`build_insert_sql`); the synthesized row for a dataset that has none keeps its `WHERE NOT
EXISTS` guard so it can never shadow a real one.

This forced one adjustment: `process_dataset_description` runs for *nested* descriptions under
`derivatives/` too, which were harmless no-ops under a guarded insert. With `OR REPLACE`, a
derivative's description would overwrite its parent's, so only the shallowest is written at all.

### 6. The synthesized `dataset_description` row survives, for a different reason

It no longer carries the root — that is `dataset_roots`' job — so it holds only `dataset_id`.
It stays because `lake.datasets()` reads this table and the wide `files` view LEFT JOINs it, so
without a row an adapter-ingested dataset would be absent from both.

## Consequences

- **The sharded workflow needs no flags.** Indexing N per-subject roots with no `--dataset-id`
  yields one dataset, N roots, and one participants list.
- **`shares_source` no longer fires between shards.** The `dataset_relations` view's
  `from <> to` drops the self-pairs once shards share an id, leaving only the real
  cross-pipeline relation.
- **`participants` is a list of participants again**, deduping on its primary key.
- **ADR 0003 §2's invariant survives verbatim** — every table is still keyed by a single
  `dataset_id`. Roots are additional, never a replacement.
- **A mistyped `--dataset-id` now merges two datasets** instead of being refused. The warning on
  a differing description is the mitigation; it is a warning rather than an error because the
  user asserting an id is a claim bidslake has no standing to overrule.
- **`(dataset_id, file_path)` is no longer unique** across a dataset's roots. Nothing in this
  ADR fixes that — every file-keyed table still assumes it. That is the subject of ADR 0006.
- **A root is registered but not characterized.** `dataset_roots` says a root exists and that
  paths resolve through it, and says nothing about whether it will still be there tomorrow — so
  a row here supports no conclusion about what has been produced. ADR 0009 adds the `tenure`
  column that does.
