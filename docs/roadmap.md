# Roadmap

What is not settled yet, and what shape the answer is likely to take. A decision becomes binding
only once it has an [architecture decision record](adr/index.md); everything on this page is ahead
of that, and dates are not promised.

## Managed mode

bidslake works today as a query engine over a dataset somebody else owns. Managed mode is the other
half: bidslake owns the storage, every piece of metadata lives in SQL, and nothing is left on disk
but the data files. Its CLI subcommands are stubs that return "not yet implemented".

- **Storage is decoupled from metadata.** A nifti's on-disk path is an opaque storage location
  bidslake assigns — it does *not* encode `sub-01`, `task-x`, or `run-02`. So metadata edits are
  pure SQL `UPDATE`s that never move files, and cross-dataset queries and aggregation come for free
  (many datasets in one database, keyed by `dataset_id`). This is the DuckLake analogy applied to
  BIDS: opaque data files plus a SQL catalog that gives them meaning.
- **Ingestion is one-way.** Standard BIDS → managed store. Exporting back to a standard BIDS layout
  is an explicit non-goal — the aim is to supplant BIDS, not to round-trip.
- **Management is per root, not per database.** One catalog holds an attached OpenNeuro dataset
  beside a managed derivative, and destructive verbs refuse to run against an attached root
  ([ADR 0007](adr/0007-root-tenure.md)).
- **The CLI acts on the store, not the metadata** (metadata is edited with SQL): `index` brings data
  under management — today's command — `verify` audits an attached root's promise and a managed
  root's integrity, and `transcode` *(stub)* changes the on-disk storage format (e.g. `.nii.gz` →
  `.nii.zst`).
- **Beyond BIDS.** The opaque-files-plus-SQL-catalog model is not BIDS-specific, and the read-only
  half already works: an adapter makes a `recon-all` or FEAT tree queryable by BIDS concept beside
  the data it came from ([ADR 0002](adr/0002-adapters-and-layouts.md)). What is still ahead is
  bringing such a tree under *management*, where bidslake owns its storage rather than only reading
  it.

## Where large tabular data lives

High-rate continuous recordings do not belong in the catalog as-is: stored one row per sample, a
single `*_physio` recording runs to millions of rows, dwarfing the metadata it accompanies.

The **mechanism** is settled. [ADR 0002](adr/0002-adapters-and-layouts.md) replaced bidslake's
hardcoded read-vs-catalog logic with the ingestion schema: selector-driven `read` / `catalog` /
`ignore` dispositions, metaschema-validated, with per-table policy alongside. Nothing about deciding
what to ingest requires new machinery — it requires writing rules.

What remains crude is the **criterion**. Two levers exist, at different granularities, and
[ADR 0004](adr/0004-undeclared-column-policy.md) is where each one's standing is argued:

- *Whole file* — the single `extension == ".tsv.gz"` → `catalog` rule. Candidate replacements — row
  count, byte size, sampling rate from the sidecar, the BIDS suffix — all have failure modes, and
  the choice interacts with the (stubbed) `transcode` and `verify` commands.
- *Per column* — `undeclared: "catalog"` in a table's policy, which stores the columns a table
  declares and leaves the rest in the file. This is what keeps fMRIPrep confounds tractable.

In managed mode the likely answer for the whole-file case is the DuckLake split applied one level
down: small tabular data stays in the catalog, while large continuous recordings are written to
partitioned Parquet (or [Vortex](https://vortex.dev/)) files on disk and exposed as views, so SQL
still sees one table.

## Reclaiming space after a re-index

Re-indexing a dataset deletes its rows and re-inserts them; DuckDB reuses the vacated blocks for
later writes but never returns them to the OS, and `CHECKPOINT` does not shrink the file. A catalog
that has been re-indexed a few times can be substantially holes — the catalog profiled for
[ADR 0004](adr/0004-undeclared-column-policy.md) was 28% free blocks, 478 MB of a 1.74 GB file.
`bidslake compact` rewrites it:

```bash
bidslake compact -d study.duckdb
```

It preserves every table, row, key, constraint, and view, verifies per-table row counts before
replacing the original, and reports what it reclaimed. An index run that leaves a substantial
fraction free says so rather than waiting to be discovered. It is deliberately not automatic: a
rewrite transiently needs twice the disk, and a first index has nothing to reclaim.

## S3

Ingesting a dataset straight from S3 works end-to-end: object listing and JSON metadata via the AWS
SDK, `.tsv` contents streamed into DuckDB via the `httpfs` extension (`read_csv` opens `s3://`
directly), and Rust-side reads (JSON sidecars, `.bval`/`.bvec`) issued concurrently to overlap
network latency. Remaining gaps are minor: dataset-embedded overlay auto-discovery
(`.bidslake/overlay.json`) is skipped for remote inputs, and the S3 integration tests are
network-gated (`#[ignore]`, run with `cargo test --test s3_ingest -- --ignored`).
