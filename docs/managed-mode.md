# Managed mode (design)

> **Status: design + stubs.** This document describes where bidslake is headed.
> The query-engine mode (read-only indexing) works today; the managed-store
> operations below are being built. CLI subcommands for them exist but currently
> return "not yet implemented".

## The idea

In query-engine mode, bidslake indexes an existing BIDS dataset and leaves the
files untouched. **Managed mode** is the endpoint: bidslake takes ownership of a
dataset and becomes its source of truth.

In a managed store:

- **Metadata lives entirely in SQL.** There are no JSON sidecars and no metadata
  TSVs on disk — only the data files (niftis) and the DuckDB database.
- **Storage is decoupled from metadata.** A nifti's path on disk is an opaque
  storage location that bidslake assigns; it is *not* a function of BIDS
  entities. Nothing on disk needs to encode `sub-01`, `task-x`, or `run-02`.
- **Metadata edits are pure SQL and never touch files.** Renaming a participant,
  fixing a value, or re-tagging a run is an `UPDATE` against the database. Since
  paths don't encode the metadata, the files stay exactly where they are.
- **bidslake still moves/rewrites files for *storage* reasons** — recompression,
  relayout — but never as a side effect of a metadata change.

This is the DuckLake analogy applied to BIDS: the data files are opaque blobs on
disk; the catalog (SQL) is what gives them meaning.

## Why this is the goal

The point of BIDS is discoverability through convention, but that convention is
exactly what makes edits expensive: the metadata is smeared across filenames,
sidecars, and TSVs. Once metadata is centralized in SQL and decoupled from
storage, the expensive operations become trivial, and cross-dataset queries and
aggregation come for free (many datasets, one database, keyed by `dataset_id`).

**Ingestion is a one-way transformation** (standard BIDS → managed store).
Exporting/materializing back to a standard BIDS layout is an explicit
**non-goal**: the aim is to *supplant* BIDS as the working format, not to
interoperate with it round-trip.

## What the CLI is for

Metadata is edited with plain SQL, so bidslake adds **no** CLI verbs for renames
or value edits. The CLI covers operations that act on the *files/store*, which
SQL cannot express:

- **`index`** — bring datasets/files under management, extending an existing
  store. (This is today's working command; the same one used for query-engine
  indexing.)
- **`verify`** *(stub)* — integrity-check the managed files (presence + checksums
  against what the catalog records).
- **`transcode`** *(stub)* — change the on-disk storage format (e.g. recompress
  `.nii.gz` → `.nii.zst`), updating the catalog's storage pointers.

A managed database also carries a **mode marker** distinguishing a read-only
index from a store bidslake owns, so destructive operations refuse to run
against a dataset bidslake does not manage.

## Longer term: beyond BIDS

The same model — opaque data files + a SQL catalog — is not specific to BIDS.
A future direction is to manage non-BIDS neuroimaging datasets as well,
supplanting extension efforts such as
[BEP043](https://bids.neuroimaging.io/extensions/beps/bep_043.html). The schema
and ingestion abstractions are kept from hard-coding BIDS-only assumptions with
this in mind. Not yet scheduled.
