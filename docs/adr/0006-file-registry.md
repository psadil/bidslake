# ADR 0006 — A real file registry, and a surrogate key for a file

Status: accepted (2026-08-12)

Relates to: `file_registry` / `all_files` (`schema/dynamic.rs`), `bids::file_id` and
`bids::Kind` (`bids.rs`), and the query surface in `bidslake-py` (`layout.py`, `binding.py`).
Supersedes ADR 0002 §6's "a primary data file lands in `scans` … a tabular file lands in
`tabular_files`" and §7's third row. Completes ADR 0005, which left `(dataset_id, file_path)`
non-unique across a dataset's roots.

## Context

ADR 0005 gave a dataset several ingest roots and closed with a debt: every file-keyed table
was keyed `(dataset_id, file_path)`, which two roots can collide on. The obvious fix — add
`root_uri` to two dozen primary keys — is what prompted the question that produced this ADR:
*why does a channels table need to know a path at all?*

It should not. But answering it required admitting something worse, which nobody had measured
before: **`scans` was not a file registry, and nothing else was one either.**

Measured on the vendored `ds000117` (2,209 walked files, 1,492 of them primary data files):

- `scans` held only primary data files, so **419 files — 19% — appeared in no table at all**:
  all 103 sidecar `.json`s, all 22 `.bval`/`.bvec`s, `dataset_description.json`, and the 293
  files of documentation, code and stimuli the dataset ships.
- The satellites did not key on it, because the file whose rows a satellite holds is the
  *tabular* file, and `scans` held none. Of the 256 files `events` draws rows from, **0** were
  rows of `scans`; of 909 `file_associations` sources, **416** were absent. A foreign key to
  `scans` would have been unsatisfiable for much of the catalog.
- `tabular_files` was a second, partial registry for the other half, with its own columns and
  no relationship to the first.

So there was no answer to "what files are in this dataset?", and no single row a
file-keyed table could point at. The concept columns were the visible symptom: with nothing to
join to, all 25 file-keyed tables carried their own copy of the same 40 regex-derived concept
expressions — **1,882 lines of generated DDL** whose only job was to make a join unnecessary.

## Decisions

### 1. `file_registry` is the walked file tree, persisted

```sql
CREATE TABLE file_registry (
    file_id HUGEINT PRIMARY KEY,
    dataset_id TEXT, root_uri TEXT, file_path TEXT,
    kind TEXT, status TEXT
    -- , projected JSON   -- only when a term map is configured (ADR 0002 §7)
);
```

**Every file the walk sees** — not every file that produced rows. A sidecar is in it; so is
`README`, `dataset_description.json`, a `.bval`, a `*_physio.tsv.gz` too large to ingest. The
one exclusion is what the walk never reached: `.bidsignore`d files, and files an `ignore`
disposition rejected. On ds000117 that is 2,209 rows against 2,447 non-hidden files on disk,
and its `.bidsignore` accounts for the 238 exactly.

This is what makes the manifest answerable, and it is also the only honest basis for a foreign
key: a table that keys on files must reference a table that has all of them.

### 2. `kind` classifies the file; `status` records what bidslake did with it

`kind` ∈ `data`, `sidecar`, `tabular`, `gradient`, `description`, `other`. It is what the old
`scans`-vs-`tabular_files` split encoded structurally, demoted to a column — which is the
whole reason one table can now hold both.

`status` ∈ `ingested`, `on_disk`, `skipped`, `failed`, absorbed verbatim from `tabular_files`.
It records the fate of a file bidslake *tried to read*, and is NULL for one it never would have
— an image has no reading to report. So `kind` says what the file is and `status` says what
became of its contents, and the pair answers from one relation what the two tables used to
answer separately and incompletely.

*Rejected:* deriving `kind` from BIDS's own `rules.files` taxonomy. Its 179 leaves look like
exactly this classification, but **149 of the 169 extension-bearing rules list `.json`
alongside a non-JSON extension**, so it cannot separate a data file from its sidecar — the one
distinction `kind` exists to draw.

### 3. The concepts live on `all_files`, a view — once, not 24 times

```sql
CREATE OR REPLACE VIEW all_files AS
  SELECT *, <sub>, <ses>, <task>, …, <datatype>, <suffix>, <extension>, <modality>
  FROM file_registry;
```

Every kind, not just data files, because the concepts are computed from `file_path` and are as
meaningful for a `*_events.tsv` or a sidecar as for the image beside it. "Data files only" is
then a `WHERE kind = 'data'` — which the Python layer spells `datafiles` and does *not*
materialize as a second view, because it is a filter, not a thing.

Three properties fall out of it being a **view over a table with no generated columns**:

- **Widening is free.** ADR 0002 §3's refusal — a later run whose overlay is wider has nowhere
  to put the difference — is retired for the concept set. A view is emitted `CREATE OR REPLACE`,
  so a wider run redefines it *retroactively, for rows already stored*. What a wider run once
  had nowhere to put, it now computes on read. (`projected` is a physical column and keeps the
  refusal; `test_registry_shape.rs` pins both halves.)
- **Narrowing is retroactive too, and that is the sharp edge.** `CREATE OR REPLACE` cuts both
  ways: a later *narrower* run — indexing plain BIDS into a catalog an adapter built —
  redefines the view without the concepts the wider one added, and without the `COALESCE` over
  `projected`. Measured: a catalog built with `--adapter fmriprep --adapter freesurfer` then
  indexed again with neither loses `from`, `to`, `mode` and `parc` from `all_files`, and its
  FreeSurfer rows read `datatype` NULL. Nothing is destroyed — the rows and their `projected`
  JSON are untouched, and re-running wide restores every answer — but the catalog is
  misleading until someone does. `check_registry_shape` cannot see it, because no *physical*
  column is missing. The remedy is the one ADR 0002 §3 already gives (name every adapter the
  catalog uses on every run); making the view additive instead is filed in `TODO.md`.
- **The bulk staged upsert can write the base table**, which a table full of generated columns
  refuses.
- **The generated Python surface lost 1,882 lines.** The 25 duplicated copies became one.

### 4. `file_id` is a surrogate key: 128 bits of SHA-256 over the identity triple

```
file_id = first 128 bits of SHA-256(dataset_id ␟ root_uri ␟ file_path)   as i128
```

Every file-keyed table is now `file_id`-keyed. This is the answer to "why does a channels
table know a path": it does not any more.

**Enforcement is partial, and deliberately so for now.** `scans`, `sidecars` and `diffusion`
declare an actual `FOREIGN KEY` to `file_registry` — they are the tables that had dangling
references, and the constraint caught a live one the day it went in (see Consequences). The
22 per-row tables and `file_associations` reference the registry by convention only.
`file_associations` cannot take one as it stands: `target_file_id` is nullable by design,
since an `IntendedFor` may name a file the catalog does not hold. The per-row tables could,
and extending it is filed in `TODO.md` rather than done here — each one is written by a bulk
`INSERT ... SELECT` from `read_csv`, so the cost has to be measured before it is imposed.

- **Computed in Rust, not by DuckDB.** DuckDB's `hash()` offers no cross-version stability
  guarantee, and this value is stored — a version bump that changed it would silently orphan
  every satellite row in an existing catalog.
- **Content-derived rather than a sequence**, so it is identical across runs and machines. A
  re-index upserts onto the same rows; two machines indexing the same tree agree.
- **The triple is the identity**, which is precisely ADR 0005's point: `root_uri` is in the
  hash, so the same relative path under two roots of one dataset gets two ids.

*Rejected:* `(dataset_id, root_uri, file_path)` as a composite key everywhere. It is the same
information, spread over three columns and 25 tables' worth of index; `root_uri` in particular
is a long absolute path repeated per satellite row.

**One trap, recorded because it is silent.** `file_id` crosses the Rust→DuckDB boundary as a
decimal *string*, not a JSON number: `serde_json` parses an integer literal too large for
`u64` as an `f64`, so a 128-bit value is rounded at *parse* time — before any code of ours sees
it. The `HUGEINT` arm of `row_values` therefore accepts a string and an `i64`/`u64` (which
widen losslessly) and refuses a `Number` it cannot prove exact. Recovery is impossible at that
point; only refusal is.

### 5. Three DuckDB constraints shaped the above

None is documented anywhere we could find; all were found the hard way:

- **A foreign key against a VIEW is refused.** So the FK target is `file_registry`, the table,
  while the concepts are on `all_files`, the view. The split is not aesthetic.
- **`CREATE VIEW IF NOT EXISTS` over an existing *table* of the same name is a silent no-op** —
  no error, no view, and every later query reads the table. This is why the registry and its
  view have different names rather than the view taking over `scans`.
- **`INSERT OR REPLACE` fails on a table carrying more than one UNIQUE/PK constraint**, which
  is why `file_id` is the *sole* key and the identity triple is not re-asserted as a second
  UNIQUE — the hash already guarantees what that constraint would have.
- **A table cannot be replaced by a same-named view.** The other half of the second point:
  `CREATE OR REPLACE VIEW` over a table errors outright, and `IF NOT EXISTS` no-ops. So a
  shipped table's *name* is effectively frozen — worth knowing before shipping a table you
  may later want to compute. (Found while turning `diffusion` into a view, ADR 0007 §2.)

### 6. `scans` becomes what its name says: the `scans.tsv` satellite

It keeps `acq_time`, `HED`, and `other_data`, keyed by `file_id` — one row per data file
*that a `scans.tsv` describes*, built the way `sessions` is built from `sessions.tsv`. It is
no longer a registry, no longer the thing `get()` iterates, and no longer where concepts live.
A dataset that ships no `scans.tsv` has no rows in it at all.

That last part took a deletion, not just a rename. `scans` was seeded with a stub row per
discovered data file, for one reason: to give the `sidecars` foreign key something to point
at. §4 moved that key to `file_registry`, which holds every file whether or not a `scans.tsv`
mentions it — so the seeding had no remaining purpose, and what it left behind were rows
claiming to describe acquisitions nothing had described (80 all-NULL rows on `ds001`).

`tabular_files` is **deleted**. Its file rows are registry rows; its `status` is a registry
column. Its `n_rows` is not preserved: it counted rows contributed *per run*, which upsert
semantics (commit `5ad965c`) made meaningless.

*Rejected:* renaming `scans` to `file_registry` in place. `scans.tsv` is a real BIDS file with
a real table, and taking its name for the manifest would leave the BIDS concept homeless.

### 7. A query against a satellite still takes concept filters

The satellites lost their concept columns; they did not lose the ability to be queried by
concept. `BidsLake._relation` joins a file-keyed table back to `all_files` on `file_id`, and
`_filter_columns` reports the union — so `lake.get(table="scans", suffix="bold")` and
`TableInput(join=("sub","task","run"), table="events")` work exactly as before, against tables
that store neither `sub` nor `suffix`.

This is what makes §3 a storage decision rather than an API break. `columns()` stays a faithful
report of what the database holds; the join is what the query layer does with it.

## Consequences

- **The manifest exists.** `lake.all_files` answers "what is in this dataset" for the first
  time, including the 19% that was previously in no table at all.
- **Referential integrity became possible, and is enforced where it had already failed.** A
  foreign key needs a target that actually contains every file, which is new. `sidecars`,
  `scans` and the gradient tables declare one, and it caught a live bug the day it went in:
  `ds114`'s root-level inherited `dwi.bval`/`dwi.bvec` had been writing `diffusion` rows keyed
  to a synthesized `dwi.nii.gz` that does not exist on disk. Those were skipped as a stopgap;
  ADR 0007 resolved the underlying many-to-many, and the gradient payloads now key on the
  gradient file itself, where the constraint is satisfiable by construction. The per-row
  tables are still not covered (§4).
- **ADR 0002 §7's third row is gone.** There is no "virtual falling back from a stored
  projection" *table*; the `COALESCE` over `projected` is a select item of `all_files`. The
  precedence rule and the derived projectable set are unchanged.
- **ADR 0002 §3's widening refusal narrows** to the `projected` column alone (§3 above).
- **`(dataset_id, file_path)` is unique again where it matters**, because nothing is keyed on
  it. ADR 0005's closing debt is discharged.
- **`row_idx` is unaffected.** It remains on exactly the tables whose source order is
  meaningful (commit `21860ef`); `file_id` replaces the path half of those keys only.
