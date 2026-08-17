# ADR 0007 — Root tenure, and what the catalog is allowed to conclude

```
ADR: 0007
Title: Root tenure, and what the catalog is allowed to conclude
Status: Provisional
Type: Design
Created: 15-Aug-2026
Requires: 0005
```

## Abstract

Each root in `dataset_roots` carries a `tenure`, `attached` or `managed`: a catalog is an index,
never an owner, unless a root says otherwise — a bound on conclusions, not on reads. An `attached`
root's rows are a sound work-*finder* and an unsound work-*skipper* until a `stat` or
`bidslake verify` confirms them.

## Motivation

A pipeline that indexes its own output and then asks the catalog which units are finished, pointed
at an empty directory that has never been written to, reports every step complete and exits 0. The
immediate cause is a missing predicate: the query filters on `dataset_id` and never on `root_uri`,
so it answers about the *indexed dataset* while the caller is asking about the *directory being
written to*. Adding the predicate fixes one caller and leaves the next to repeat the mistake, so the
prior question is the one worth asking: is a catalog the right thing to ask "is this work done" at
all?

DuckLake's catalog **is** authoritative about which files are real, and earns that two ways. It is
the commit point — "Data files are written first, before any catalog metadata is updated"
(*DuckLake: The Definitive Guide*, Ch. 3, p. 47), and only that later metadata transaction makes
them visible to readers — so a file on disk that is not in the catalog is not *stale*, it is *not a
member*. And it owns them: registering an externally-written file copies nothing but transfers
ownership, and DuckLake's compaction may then delete it.

bidslake has neither. It indexes out of band, after the fact, over trees it did not write — an
OpenNeuro dataset, a colleague's fMRIPrep output, a study tree assembled by hand. So its two error
directions are not symmetric:

| | DuckLake | bidslake, an `attached` root |
|---|---|---|
| on disk, not in catalog | correctly invisible (an orphaned write) | a redundant re-run — costs time |
| in catalog, not on disk | impossible without corrupting the lake | routine: purged scratch, wrong destination, a deleted subject |

DuckLake never has to index a tree it did not write, so it needs no vocabulary for one. The missing
concept is not global ownership but **what was promised about a particular root**, which differs
from root to root inside one catalog. [ADR 0005](0005-multi-root-datasets.md) makes roots plural and
registers where each one is, without saying what may be concluded from its rows.

## Rationale

### On the tiers, and on narrowing versus confirming

Two tiers rather than one because DuckLake already names the transition:
[`ducklake_add_data_files`](https://ducklake.select/docs/stable/duckdb/metadata/adding_files)
registers files written by someone else and moves ownership. `attached` is permanent because it is
how bidslake stays useful to somebody with data and no intention of handing over control, and most
catalogs hold nothing else.

"Sound as a work-finder, unsound as a work-skipper" is then DuckLake's own two-stage pruning
contract (p. 55): the catalog decides which files to examine, and those files decide which row
groups to scan. The catalog narrows; storage confirms. The failing query in Motivation used it in
the unsound direction. `verify` audits that stated promise rather than scanning for whatever has
stopped being true: an `attached` root that fails it has had its contract broken by somebody, a
`managed` root that fails it means bidslake has a bug.

### On the boundary with a pipeline's own bookkeeping

The criterion is not "is this file temporary" but **who is accountable if it disappears**: if the
run that made it will simply remake it, the file belongs to the workflow engine; if nobody will and
somebody needs to notice, it belongs here. Dagster, nipype and Snakemake each keep a completion
ledger and a working tree; those systems have both a completion model *and* a recovery model, and a
catalog row adds only a second authority that can disagree with the first. §3 still declines to
enforce the line, because it is about what a file is *for* — which the person indexing knows and
bidslake does not — and because it is temporal rather than spatial: a file becomes bidslake's
concern when the run that could have rebuilt it is over.

Three things go wrong when ephemeral files get in. `verify` drowns, and a check whose output is
mostly expected failures is not a check. `file_registry` stops answering the question
[ADR 0006](0006-file-registry.md) built it for, "what files are in this dataset". And under
`managed` tenure, relocating or transcoding a file an engine is midway through writing is
corruption, so the tenure and ephemerality boundaries have to coincide.

The hard case — a FEAT tree's `filtered_func_data.nii.gz` is both a step input and a scientific
product — dissolves rather than needing adjudication, because the line is drawn externally.
[fMRIPrep since 23.2.0](https://fmriprep.org/en/stable/usage.html) accepts precomputed derivatives
via `--derivatives`/`-d`, on the condition that they follow BIDS Derivatives conventions:
entity-named, with a `dataset_description.json`. That file is bidslake's indexing precondition too,
and is how a pipeline says "this is a product now", so writing it is the natural moment to declare
tenure — a FEAT output tree ships none, and nothing under `work/` has one. Each adapter's ingestion
fragment tracks whatever its pipeline formalizes, which is why bidslake keeps no list of its own:
fMRIPrep calls derivatives reuse **experimental** and enumerates no accepted set.

## Specification

### 1. Tenure is a property of a root, not of a database

`dataset_roots` carries a third column beside its `(dataset_id, root_uri)` primary key:
`tenure TEXT NOT NULL DEFAULT 'attached' CHECK (tenure IN ('attached', 'managed'))`.

| Tenure | Who writes there | The catalog may conclude | Destructive verbs |
|---|---|---|---|
| `attached` (default) | somebody else | "this was here when I looked, and is still there unless `verify` says otherwise" | refused |
| `managed` (`index --managed`) | bidslake | "this is here" — the catalog is the commit point | allowed |

Both tiers are permanent. Nothing in the read path depends on tenure; only conclusions do.

Reading tenure back neither requires the column to exist nor trusts its domain.
`BidsDb::dataset_root_tenure` and `BidsLake.roots` consult `duckdb_columns()` first and report
`attached` when `dataset_roots` has no `tenure` column, so such a catalog opens instead of failing
with a `Binder Error`, and `roots()` selects `'attached' AS tenure` so the frame's shape is the same
either way. The token mapping is asymmetric on purpose: `managed` is recognized and anything else —
a hand-edited value, or one the migration below added without the `CHECK` — degrades to `attached`
rather than failing the query.

### 2. Tenure is asserted per run, and omission is not retraction

`--managed` upserts (`ON CONFLICT (dataset_id, root_uri) DO UPDATE SET tenure = 'managed'`); the
`attached` default `DO NOTHING`s. A later plain re-index therefore leaves an already-managed root
managed, and asserting `--managed` over an already-indexed tree brings it under management without
a re-index from scratch. A root that was never registered has **no** tenure —
`dataset_root_tenure` returns `None`, not a defaulted `attached`.

### 3. Durability is declared by the person indexing; bidslake does not infer it

Indexing a root is a statement that its files will still be there. bidslake takes that statement and
does not audit it — in particular, it does not refuse a root for looking temporary. Where a root
lives is the indexer's business.

### 4. A completion query is scoped to a root, and an attached answer needs confirming

`root_uri` is the only thing that says *which tree* a row is about, so a query about work already
done joins `dataset_roots` and is scoped by root — `WHERE root_uri = ?`. `bidslake.to_uri` turns an
output directory into a value that predicate matches, resolving symlinks because the ingester
canonicalizes the root once before recording it (on macOS `/tmp` is `/private/tmp`, and the
unresolved spelling matches no row).

For an `attached` root the catalog's answer is a claim about a past observation, and turning it into
a decision to *skip* work needs confirmation: a `stat` of the files in question, or a
`bidslake verify` that has passed since the last index.

### 5. A pipeline's own working state is not indexed, and that line is drawn per file

Tenure draws the per-root line; an adapter's ingestion fragment draws the per-file one, with
`disposition: ignore`. `crates/bids-schema/data/ingestion/freesurfer.json` opens with
`{ "selectors": ["match(path, \"/touch/\")"], "disposition": "ignore" }` — recon-all's `touch/` is
its own step-completion ledger — and `feat.json` opens with three more, ignoring `fix/`, `mc/`,
`filtered_func_data.ica/` and `pyfix.log`. Naming a file and admitting it to the catalog are
separate decisions: `feat`'s *term map* maps those same paths (`fix/[^/]+` carries
`desc: fixscratch`, `pyfix.log` carries `desc: log`); the ingestion fragment is what keeps them out.

### 6. `managed` gates the verbs that move or rewrite files

The capabilities tenure buys are `transcode` (change a file's on-disk storage format), relocation
(move the files and rewrite `root_uri` and `file_path` in one transaction), garbage collection, and
the opaque assigned storage paths the roadmap's managed-mode design describes. Each is gated on
`managed` and refuses against an `attached` root. None is implemented; `transcode` is a stub that
says so.

## Backwards Compatibility

Nothing breaks, and no catalog needs re-indexing. `CREATE TABLE IF NOT EXISTS` leaves an existing
two-column `dataset_roots` alone, so `create_tables` follows it with an idempotent `ALTER TABLE
dataset_roots ADD COLUMN IF NOT EXISTS tenure TEXT DEFAULT 'attached'`. That DDL runs only under
`index`; every read command runs none, and the Python bindings open the catalog read-only and
cannot — which is why §1's read path tolerates a catalog that has never run it. The default is
honest rather than convenient: a root registered without asserting anything promised its files were
there and nothing more, which is what `attached` means.

Indexing gains no new way to fail: tenure adds a column and a flag, rejects nothing that was
accepted before — so no existing caller needs an escape hatch — and leaves `sibling()`, `LayoutAt`,
discovery queries and the wide views untouched.

## Rejected Ideas

**Refusing roots that look temporary.** A prefix list — `$TMPDIR`, `/tmp`, `/var/tmp`,
`/var/folders`, `/scratch` — with an `--allow-transient` override. It contradicts §3: authority over
a root is *declared* precisely so that it is not inferred, and a path prefix is the same inference
by another route and a weaker signal than a flag. It is also wrong constantly — `/scratch` is
durable project space at many sites, containers routinely mount real storage under `/tmp`, `$TMPDIR`
is whatever a scheduler set it to — so the list can only be a heuristic imposed on people who know
their own filesystem better than it does, and being wrong costs upkeep: on macOS `/var` is a symlink
to `/private/var`, so every prefix has to be matched twice, and every test harness that indexes into
a temp dir needs an escape hatch. And it buys nothing: the failure in Motivation is a query that did
not say which tree it meant, not a root in the wrong place.

**A bidslake-invented rule that indexing is a promise.** "You may not both index a file and reap it
as scratch" does resolve the expensive-intermediate case, but it makes bidslake the author of a
criterion no producing pipeline uses; fMRIPrep's `--derivatives` precondition is the same line drawn
by someone with standing to draw it.

**A per-database mode marker.** One catalog holds an attached OpenNeuro dataset beside a managed
derivative, so a database-wide flag cannot state the thing that differs.

**A separate table for a root's authority.** `(dataset_id, root_uri)` already means three things at
once: where a file resolves from ([ADR 0005](0005-multi-root-datasets.md)), two thirds of the
identity triple `file_id` is hashed over ([ADR 0006](0006-file-registry.md)), and what may be
concluded from the root's rows. A lot for one pair to mean, and still better than a second place to
look for "is this root real."

**`INSERT OR REPLACE` for root registration.** `--managed` is an assertion; its absence on a later
run is not a retraction. Replacing the row would let every routine re-index silently demote a
managed root and withdraw the authority its rows carry.

**Defaulting an unregistered root to `attached`.** Conflating "never indexed" with "indexed,
promising only durability" is how a query about a tree nobody indexed comes back looking
authoritative — the failure in Motivation, by another route.

## Open Issues

- **A catalog snapshot id, so a claim about it can be dated.** Tenure establishes that a claim was
  made in good faith over a durable root but does not date it, so a consumer wanting to skip work
  has to re-check the filesystem itself. DuckLake's answer is a `snapshot_id`/`snapshot_time`
  catalog table plus `begin_snapshot`/`end_snapshot` on the file table, turning "as of" into one
  predicate and letting `verify` record its result for a caller on a node that never mounted the
  tree. Re-indexing is `DELETE` + re-insert today, so this is real work, not a column.
- **Which managed verb is built first.** Relocation is the direct answer to "the output has moved
  from scratch to shared storage", which today has no supported fix but a re-index.
- **A rewrite that preserved size and mtime is invisible to `verify`.** It compares the registry's
  `size_bytes`/`mtime_ns` against the tree, which catches the deletion, truncation, replacement and
  rewrite a derivative tree actually suffers, and misses a forgery, or a second write inside one
  tick on a filesystem with coarse mtime. Closing it means a checksum, and hashing reads every byte
  where a stat reads none — an index that read a whole study to build itself would not be an index —
  so it belongs behind a flag on `verify`, hashing on demand the files whose stat already matched,
  rather than in the ingest.
