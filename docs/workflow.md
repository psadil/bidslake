# Example workflow

This walkthrough shows the everyday tasks bidslake makes easy: selecting files by
metadata, and editing metadata with a single statement instead of touching many
files. Every query below was run against a real dataset
(`tests/bids-examples/ds001`, the Balloon Analog Risk-taking Task).

## 0. Build a database

```bash
cargo run --release -- index \
    --input tests/bids-examples/ds001 \
    --output ds001.duckdb

duckdb ds001.duckdb
```

Everything after this is plain SQL — use the `duckdb` CLI, the Python
`duckdb` package, or any DuckDB client.

## 1. Select files by participant metadata

The classic "give me files for participants under 30". A scan belongs to a
participant via its path prefix, so join on that:

```sql
SELECT p.participant_id, p.age, s.file_path
FROM participants p
JOIN scans s
  ON s.dataset_id = p.dataset_id
 AND s.file_path LIKE p.participant_id || '/%'
WHERE p.age < 30
ORDER BY p.participant_id, s.file_path;
```

```
sub-01  26.0  sub-01/anat/sub-01_T1w.nii.gz
sub-01  26.0  sub-01/func/sub-01_task-balloonanalogrisktask_run-01_bold.nii.gz
...
```

To get just the count of matching files:

```sql
SELECT COUNT(*)
FROM participants p
JOIN scans s
  ON s.dataset_id = p.dataset_id
 AND s.file_path LIKE p.participant_id || '/%'
WHERE p.age < 30;   -- 75
```

## 2. Select files by acquisition metadata

Filter on sidecar (JSON) metadata — e.g. bold files acquired with a 2-second TR:

```sql
SELECT s.file_path, sc.repetition_time
FROM scans s
JOIN sidecars sc USING (dataset_id, file_path)
WHERE sc.repetition_time = 2.0
  AND s.file_path LIKE '%bold.nii.gz';
```

Because BIDS inheritance is applied at ingest, this works even when
`RepetitionTime` was only declared once in a dataset-level
`task-*_bold.json` — every matching scan's sidecar carries the inherited value.

Non-standard metadata fields land in `other_data` and are queryable with
DuckDB's JSON operators:

```sql
SELECT file_path, other_data->>'CustomField' AS custom
FROM sidecars
WHERE other_data->>'CustomField' IS NOT NULL;
```

## 3. Inspect all metadata for one file

```sql
SELECT * FROM sidecars
WHERE file_path = 'sub-01/func/sub-01_task-balloonanalogrisktask_run-01_bold.nii.gz';
```

Known BIDS fields appear in their dedicated columns (`repetition_time`,
`echo_time`, `flip_angle`, …); anything non-standard is in `other_data`.

## 4. Rename a participant

In raw BIDS this means renaming directories and rewriting `participants.tsv`,
`*_scans.tsv`, and every filename. With bidslake, the metadata-layer edit is one
statement:

```sql
UPDATE participants
SET participant_id = 'sub-001'
WHERE dataset_id = 'Balloon Analog Risk-taking Task'
  AND participant_id = 'sub-01';
```

Wrap several related edits in a transaction if you like:

```sql
BEGIN;
UPDATE participants SET participant_id = 'sub-001' WHERE participant_id = 'sub-01';
UPDATE sessions     SET participant_id = 'sub-001' WHERE participant_id = 'sub-01';
COMMIT;
```

> **Query-engine vs managed mode.** In a read-only index of an existing BIDS
> dataset, the `file_path` columns still encode the on-disk name (`sub-01/…`);
> the edit above changes the metadata layer, not the files. In **managed mode**,
> file storage is decoupled from these labels, so the single `UPDATE` *is* the
> whole rename — nothing on disk moves. See
> [managed-mode.md](managed-mode.md).

## 5. Correct or fill in values

Same idea for fixing data-entry errors or adding metadata:

```sql
-- Fix an age
UPDATE participants SET age = 31 WHERE participant_id = 'sub-02';

-- Set handedness for everyone missing it
UPDATE participants SET handedness = 'R' WHERE handedness IS NULL;
```

## 6. Audit across the dataset

Aggregate queries that would otherwise require a script over every sidecar:

```sql
-- Distinct repetition times in use, and how many scans use each
SELECT repetition_time, COUNT(*) AS n
FROM sidecars
WHERE repetition_time IS NOT NULL
GROUP BY repetition_time
ORDER BY n DESC;

-- Which scans have a fieldmap intended for them?
SELECT target_file_path, source_file_path
FROM file_associations
WHERE association_type = 'fieldmap';
```
