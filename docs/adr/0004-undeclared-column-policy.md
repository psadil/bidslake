# ADR 0004 — Storage is a policy, not an invariant

```
ADR: 0004
Title: Storage is a policy, not an invariant
Status: Provisional
Type: Design
Created: 30-Jul-2026
Requires: 0001, 0002, 0006
```

## Abstract

A table stores what its schema declares, not what its files contain. `other_data`, the JSON overflow
column, is conditional: a tabular table declaring `undeclared: catalog` has none and records the
dropped names in `tabular_undeclared_columns`; a scoped policy keeps it and records nothing.

## Motivation

`other_data` preserves every source field a table has no dedicated column for. An fMRIPrep
`desc-confounds_timeseries.tsv` carries ~1,800 columns, of which the bundled fmriprep overlay
declares 13 (six motion parameters, `framewise_displacement`, `dvars`, `csf`, …); the rest, mostly
aCompCor components, become one JSON object per volume.

Profiled 2026-07 on 12 fMRIPrep derivative datasets, 2 subjects each, 48 confounds files — ~70,000
rows in 1.74 GB. `fmriprep_confounds.other_data` was 1,160 MB of it: 97.3% of used space, ~56 KB per
JSON object, ~24 MB per confounds file; `sidecars.other_data` another 16.8 MB. At participants ×
sessions × runs, a multi-session study reaches terabytes from confounds alone.

The dial that exists is whole-file: [ADR 0002](0002-adapters-and-layouts.md)'s ingestion rules give
a file `disposition: read | catalog | ignore`. A confounds file has to be `read` — motion
parameters, FD and DVARS are the point of it — and nothing in that model, or elsewhere in the BIDS
schema, says "read this file, but not those columns".

## Rationale

Measured (release build) on a dataset rebuilt from that catalog (8 confounds files, 1,798 columns ×
450 rows, real 2,151-key sidecars): 207.26 MB in 829 blocks with the fmriprep overlay alone, 8.76 MB
in 35 blocks once `--adapter fmriprep` adds the ingestion fragment — **23.7× smaller**. At 16
subjects (64 files, 28,800 rows) it is still 8.7 MB — DuckDB's 256 KB per-segment minimum and the
schema blob, not data. No size/speed trade: the policy only shrinks the SELECT list.

`sidecars` is one table for every file in the catalog, so a table-wide flag there would strip custom
metadata from all raw BIDS to reach one derivative's sidecars. Scoped, it drops fMRIPrep's
`desc-confounds_timeseries.json` — a ~366 KB dictionary of ~2,200 confound descriptions per BOLD
run — while an ordinary `_bold.json` beside it keeps its custom fields, and costs a plain BIDS
ingest nothing: only a table declaring `undeclaredWhen` has selectors evaluated.

The two forms are enforced at different points. On the tabular path, omitting the column rather than
nulling it is load-bearing: `row_values` iterates `table_columns`, so with no `other_data` field
there is no branch to populate. `sidecars` keeps its column either way, so `build_sidecar_row`
filters keys as it flattens. Declaring `a_comp_cor_00` in the overlay stores it again, at 8 B/row.

The name dictionary is the compensation for dropping the values: it answers "what confound
regressors does this catalog's data contain?" without opening a file.

## Specification

### 1. A table stores what its schema declares

A tabular table's ingestion policy may declare `undeclared: catalog`: the table is created with no
`other_data` column, and the columns its schema does not declare are not stored. The default `store`
preserves them there. `sidecars` and `dataset_description` always get the column. `base.json`
declares no `undeclared` policy, because `emit-types` renders the Python column tables from
`Ingestion::base()`.

### 2. Two forms — table-wide and selector-scoped

`undeclared` is table-wide and static, so it can drive DDL. `undeclaredWhen` is a list of
`{selectors, undeclared}` entries resolved per file — first match wins, then the table-wide
`undeclared`, then `store` — and does *not* change the table's shape. It is resolved where a row is
built from one file's parsed JSON: `sidecars`. Entries append rather than replace across fragments,
so several adapters can each scope a policy onto it.

### 3. The names not stored are recorded globally, by name

`tabular_undeclared_columns (table_name, name)` holds one row per distinct name per table for the
whole catalog, written `INSERT OR IGNORE` as files are read, and nothing per file. Only a tabular
table whose *static* policy is `catalog` records; a scoped policy drops keys without recording them.

### 4. Losslessness is by reference

The file is parsed, not consumed: it stays on disk and keeps its `file_registry` row, whose
`file_path` is the authoritative record of its full column set ([ADR 0006](0006-file-registry.md));
`BidsLake.resolve` turns that row into an openable handle. Under a scoped `catalog` policy,
`BidsFile.metadata` carries the declared fields only.

## Backwards Compatibility

Nothing breaks. A catalog first built under `store` and re-indexed with a fragment flipping a
tabular table to `catalog` keeps the column — tables are created `IF NOT EXISTS` — but stops
writing it, and the pre-insert `DELETE` is per `file_id`, so refreshed rows go NULL while rows from
roots not re-indexed keep what they held. Re-index every root of the dataset, or drop the column.

## Rejected Ideas

**`additional_columns: "not_allowed"` on the tabular rule.** The BIDS-native field, and the wrong
lever — a *validation* assertion, not a storage one. Setting it makes `bids-validator-rs` emit
`TSV_ADDITIONAL_COLUMNS_NOT_ALLOWED` for every real fMRIPrep confounds file; the fmriprep and
qsiprep overlays declare `additional_columns: "allowed"`, which is the truth about those files.

**A denser encoding of the overflow.** DuckDB stores the JSON column `Uncompressed`: at ~56 KB every
value is an overflow string, so dictionary and FSST encoding never apply, and 53% of each object is
repeated key names. Against the profiled `fmriprep_confounds` table (21,332 rows):
`MAP(VARCHAR, DOUBLE)` 267 MB, a long/EAV `(file, row, name, value)` table ~350 MB, all ~1,800
columns as real `DOUBLE`s ~280 MB — 4.3×, 3.3× and 4.1× smaller than the same table's 1,160 MB as
JSON, clustering at 7.0–7.4 bytes per value against 8 raw, near what 38M floats simply cost.
Encoding buys a constant factor, not a change of asymptote; only declining to store the values does.

**A per-file manifest of the undeclared columns.** Keying one by header signature looks free until
the headers are counted: the 48 profiled confounds files carry 38 distinct headers, the aCompCor
component count varying per run, so the manifest grows with the study at about one header per file.
The *names* are bounded by the pipeline's vocabulary, not by file count — a few thousand, tens of
kilobytes.

**A different storage substrate.** At tens of GB a single DuckDB file is unremarkable, and no
substrate addresses what this one does: a terabyte of floats is a terabyte in Parquet too.
DuckLake's argument here is operational, not about size.

## Open Issues

**The whole-file boundary is still crude.** `extension == ".tsv.gz"` → `catalog` is the only rule
keeping high-rate recordings out of the row store, and it is a proxy — BIDS happens to compress them
— not a principle: stored one row per sample, a `*_physio` recording runs to millions of rows.

**A scoped policy reaches `sidecars` only, and records nothing.** The tabular read paths consult the
static `undeclared`, so an `undeclaredWhen` entry on a tabular table validates and does nothing; and
`record_undeclared_columns` is called only from those paths, so the names a scoped policy drops go
unrecorded. Either it should reach the tabular readers and the dictionary, or the metaschema should
say the field is honoured on `sidecars` alone, as `ignoreKeys` already does.
