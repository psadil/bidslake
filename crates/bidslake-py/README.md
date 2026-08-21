# bidslake (Python)

Typed querying of [bidslake](https://github.com/psadil/bidslake) datasets — the BIDSLayout /
bids2table analog. Open the DuckDB catalog a `bidslake index` produced and query it by BIDS
concept, getting back Polars or an iterable of file handles.

This page is how to *use* the API. Why each piece is shaped the way it is — the storage policy
behind a wide table, cross-dataset matching, the read and write directions of a layout — is in
the architecture decision records at
[github.com/psadil/bidslake/docs/adr](https://github.com/psadil/bidslake/tree/main/docs/adr),
cited below as `ADR NNNN`.

## Install

Not on PyPI yet. The package is a compiled extension, so installing it builds it — which needs a
Rust toolchain and Python 3.14 or newer:

```bash
pip install "bidslake @ git+https://github.com/psadil/bidslake#subdirectory=crates/bidslake-py"
```

You also need the `bidslake` CLI to produce a catalog in the first place; it is a separate binary
from the same repository. See the [repository README](https://github.com/psadil/bidslake#install).
For a development checkout, use `maturin develop` instead —
[CONTRIBUTING.md](https://github.com/psadil/bidslake/blob/main/CONTRIBUTING.md) has the setup.

```python
import bidslake

lake = bidslake.open("study.duckdb")

# The headline: an iterable of every resting-state fMRI file, across all
# datasets in the catalog. `get` iterates the whole registry, so say which slice
# you mean -- an image and the sidecar beside it share every entity.
for f in lake.get(task="rest", suffix="bold", extension=".nii.gz"):
    img = nib.load(f.local_path)     # resolved from root_uri
    tr = f.metadata["RepetitionTime"]  # sidecar metadata, BIDS-cased
    events = f.get_events()          # associated events (inheritance-resolved)

# Whole tables as Polars (eager or lazy with projection pushdown):
df = lake.all_files.pl()             # every file the walk saw: images, sidecars, .bval, README
lf = lake.sidecars.lazy().select("file_id", "RepetitionTime")

# Typed per-table column expressions and the wide one-big-table view:
from bidslake import C
lake.all_files.pl().filter(C.all_files.extension == ".nii.gz")   # narrow it yourself
lake.all_files.pl().filter((C.all_files.task == "rest") & (C.all_files.suffix == "bold"))
lake.files.pl()                      # registry + sidecar__*/participant__*/dataset__*/scan__*

# Safe raw SQL via t-strings:
lake.sql(t"SELECT count(*) FROM all_files WHERE suffix = {suffix}")
```

## Reading what the catalog did not store

A catalog holds what its schema declares it holds. A table whose ingestion policy is
`undeclared: catalog` keeps only its declared columns, and the file on disk stays the
record of the rest — which is how a wide derivative table like fMRIPrep's confounds stays
tractable ([ADR 0004](../../docs/adr/0004-undeclared-column-policy.md) weighs that against
storing everything).

Nothing is unreachable. `lake.all_files` is the registry — every file the ingest saw,
whatever it did with it — and `lake.resolve()` opens any of them:

```python
import polars as pl

# What names exist but are not stored — no disk access.
lake.table("tabular_undeclared_columns").pl()

# And the values, from the file itself.
row = (lake.all_files.pl()
       .filter(pl.col("file_path").str.contains("confounds.tsv"))
       .row(0, named=True))
path = lake.resolve(row["dataset_id"], row["file_path"], row["root_uri"])
full = pl.read_csv(path.open("rb"), separator="\t", null_values="n/a")
full.select("^a_comp_cor_.*$")
```

Pass `root_uri` when a dataset spans several ingest roots
([ADR 0005](../../docs/adr/0005-multi-root-datasets.md)): the root a file came from is what
says where to open it, and every registry row carries it.

`resolve()` uses the same root resolution as `BidsFile.path` (so `base_dir` and
`root_override` apply) and returns a `UPath`. Read through `.open()` rather than
`str(path)` — a `UPath` stringifies back to a URI, and `.open()` is what keeps this
working for a remote dataset.

To store one of those columns instead, declare it in the overlay: it then gets a typed
column and costs 8 bytes a row. Declared-ness is the dial.

## Composing queries

`get()` is one table, conjunction-only, no joins. Past that — a join, a disjunction, an
aggregate, a correlated subquery — build the statement with [SQLAlchemy
Core](https://docs.sqlalchemy.org/en/20/core/) over the generated models and hand it to
`sql()`:

```python
from sqlalchemy import select
from bidslake.schema.models import AllFiles, Sidecars

lake.sql(
    select(AllFiles.file_path, Sidecars.RepetitionTime)
    .join(Sidecars, Sidecars.file_id == AllFiles.file_id)
    .where(AllFiles.task == "rest", AllFiles.extension == ".nii.gz")
)
```

The statement is *compiled* here and executed by the same Rust engine as the other two
`sql()` forms, so it opens no second connection and comes back over the same Arrow
bridge as Polars. `sqlalchemy` is a hard dependency; `duckdb-engine` deliberately is
not, since it would pull in a second embedded DuckDB beside the one the extension
bundles.

One model per table and per view, `all_files` and `dataset_link_targets` included, named
in CamelCase (`fmriprep_confounds` → `FmriprepConfounds`). A column whose name is a
Python keyword — fMRIPrep's `from` — is reachable as `from_` and still emits `"from"`.

### Units of work

A pipeline step rarely operates on one file. It operates on a *unit* — a subject, a
session, a run — and needs several sibling files that belong to it, each matched on a
**different subset** of its entities. `sibling()` is that shape as one `LEFT JOIN
LATERAL`; everything around it stays a statement you wrote and can print.

```python
from bidslake import sibling
from bidslake.schema.models import AllFiles
from sqlalchemy import select, true

UNIT = ("sub", "ses", "task", "run")
ROLES = {
    # One mask per run, so this joins on the whole unit key. `space=None` is IS NULL —
    # what separates a native-space image from its `space-*` resamplings.
    "brain": (UNIT, None,
              {"datatype": "func", "suffix": "mask", "desc": "brain", "space": None,
               "extension": ".nii.gz"}),
    # One T1w per session, so this matches on *less* than the key.
    "anat": (("sub", "ses"), None,
             {"datatype": "anat", "suffix": "T1w", "desc": "preproc", "space": None,
              "extension": ".nii.gz"}),
    # A different dataset, reached by the name the catalog gives it.
    "wmparc": (("sub", "ses"), "freesurfer", {"seg": "wmparc", "extension": ".mgz"}),
}

a = AllFiles.__table__.alias("a")
cols = [a.c[k] for k in UNIT] + [a.c.dataset_id, a.c.root_uri, a.c.file_path]
frm = a
for name, (join, via, where) in ROLES.items():
    lat, sel = sibling(a, name, join, where, via=via)
    cols += sel
    frm = frm.outerjoin(lat, true())

units = lake.sql(select(*cols).select_from(frm).where(
    a.c.extension == ".nii.gz", a.c.datatype == "func", a.c.suffix == "bold",
    a.c.desc == "preproc", a.c.space.is_(None),
))
```

`where` is a mapping rather than keyword arguments because `from` is a Python keyword and
fMRIPrep uses it as an entity. Pin `extension` in every role — `all_files` is the whole
registry and every image has a `.json` sidecar carrying identical entities, so a role
that does not discriminate matches two files and reads as ambiguous.

One row per unit, every sibling beside it, in **one** query — not one per role per unit.
DuckDB decorrelates the laterals (`EXPLAIN` shows `HASH_JOIN` + `HASH_GROUP_BY`, no
nested loop).

The `__n` columns are the point. A per-unit gap is data, not an exception, and the three
outcomes are distinguishable: `1` resolved, `0` missing (that subject is incomplete),
`2+` ambiguous (the filter under-specifies — e.g. joining on `sub` alone across datasets
whose subject labels collide). Incomplete subjects are visible before anything is
submitted.

`sibling()` writes those columns and `unresolved()` / `sibling_path()` read them back:

```python
from bidslake import sibling_path, to_local_path, unresolved

for row in units.iter_rows(named=True):
    if bad := unresolved(row, ROLES):        # {} when every role resolved
        print(row["sub"], bad)               # {"wmparc": 0} missing, {"anat": 2} ambiguous
        continue
    work(to_local_path(sibling_path(lake, row, "anat")))
```

`sibling_path` takes the whole row because a path only means something together with the
root it was walked from — the three location columns are one answer, and it applies the
same `base_dir`/`root_override` rebasing as `lake.resolve`. Passing no name reads the
unprefixed columns, i.e. the anchor. It returns a `UPath`; `to_local_path` is the local
catalog's shortcut.

Adding filters to the anchor is what a scheduler launching one job per unit wants — it
narrows the anchor query rather than the result, so the other units are never resolved
at all. Narrow the *table-slice* query below by the same anchor while you are at it: one
ingested confounds file is associated with every image it describes, so an unrestricted
fetch pulls the `space-*` resamplings your pipeline never opens.

### `via` names a link, not a dataset

`via=` joins `dataset_link_targets` to resolve what *this catalog* calls `freesurfer`,
from the dataset's BIDS `DatasetLinks` or from `bidslake link alias`. The same query then
works against any catalog defining the name, which a hardcoded `dataset_id` cannot — ids
are free text, and a study processed one subject at a time has one dataset *per subject*
(`sub-01-freesurfer`, …) with no single id to write down.

The name is resolved **in the anchor's own dataset**, which is what BIDS `DatasetLinks`
already means. So a sharded study needs no ceremony — each shard's description already
says which tree is its own — and, deliberately, a link declared in a *neighbouring*
dataset is not in scope.

```bash
bidslake link alias -d study.duckdb --dataset fmriprep --as freesurfer --target ../freesurfer
bidslake link list  -d study.duckdb    # what every link name resolves to
```

Use it. Dropping `via` and matching on entities catalog-wide is unsound in a way the `__n`
check cannot catch: a file missing from one study can be silently answered by another
study's, and one match never looks ambiguous
([ADR 0003](../../docs/adr/0003-associations.md)).

### Rows, not files

A sibling is not always a file. Motion regressors are a *slice of an ingested table*,
keyed to the image they describe through `file_associations` — hundreds of rows per unit,
so a second query rather than another lateral column, and still one query for the whole
study:

```python
from bidslake.schema.models import FileAssociations, FmriprepConfounds

MOTION = ("rot_x", "rot_y", "rot_z", "trans_x", "trans_y", "trans_z")
fa, t = FileAssociations.__table__.alias("fa"), FmriprepConfounds.__table__.alias("t")
motion = {
    key[0]: frame.select(MOTION)
    for key, frame in lake.sql(
        select(fa.c.source_file_id.label("file_id"), *[t.c[c] for c in MOTION])
        .select_from(t.join(fa, fa.c.target_file_id == t.c.file_id))
        .where(fa.c.association_type == "fmriprep_confounds")
        .order_by(t.c.row_idx)
    ).partition_by(["file_id"], as_dict=True).items()
}
```

Keyed through the association rather than by matching entities. A confounds file happens
to share `sub`/`ses`/`task`/`run` with the images it describes, so an entity match gets
the right answer — but only by luck of naming, and it is simply wrong for a describing
file one directory level up, which has no `sub` to join on. The associations come from
the BIDS schema's own `meta.associations`
([ADR 0003](../../docs/adr/0003-associations.md)).

### Querying an augmented catalog

The bundled models and `GetFilters` are pinned to the BIDS schema this build ships, so a
query over an *overlay-augmented* catalog — fMRIPrep's `from`/`to` and `xfm`, a
FreeSurfer adapter's `seg`, an ingested confounds table — would be flagged column by
column against a vocabulary that does not contain those words. Generate the catalog's own
and import from it instead; nothing else about the query changes:

```console
$ python -m bidslake.stubgen study.duckdb --out _bids_types.py
```

```python
from _bids_types import AllFiles, FmriprepConfounds   # instead of `bidslake.schema.models`

select(AllFiles.file_path).where(AllFiles.from_ == "T1w", AllFiles.to == "MNI152NLin6Asym")
```

### Scheduling

A query is only a query; bidslake schedules nothing. The same statement drives a `for`
loop, a process pool, a SLURM array, or a Snakemake input function:

```python
import submitit

ready = [r for r in units.iter_rows(named=True) if all(r[f"{x}__n"] == 1 for x in ROLES)]
executor = submitit.AutoExecutor(folder="logs")
executor.update_parameters(timeout_min=240, slurm_partition="normal", cpus_per_task=4)
jobs = executor.map_array(run_one_unit, ready)   # one job per unit
```

## Layouts: naming an output before it exists

The queries above resolve what a unit *consumes*. A layout is the other direction —
where its outputs go. Nothing can query for a file a pipeline has not written yet, so without one
every consumer hardcodes the convention, which is how a wrapper grows two dozen properties
that are only string joins.

```python
out = bidslake.layout("feat").under(dst / stem)
out["highres2standard_mat"]   # <dst>/<stem>/reg/highres2standard.mat
out["filtered_func_clean"]    # <dst>/<stem>/filtered_func_data_clean.nii.gz
out.mkdir("melodic_mix")      # the same, with the parent directory created
```

A few roles are not determined by the root alone — `feat`'s two `classification` roles render
`fix4melview_{training}_thr{threshold}.txt`. Bind those placeholders where the root is bound, and
every role is then reachable by name:

```python
out = bidslake.layout("feat").under(dst / stem, training="UKBiobank", threshold="1")
out["classification"]                                # fix4melview_UKBiobank_thr1.txt
out.path("classification_by_rater", rater="psadil")  # merged over the above; a keyword wins
```

A placeholder nothing has bound still raises rather than rendering a plausible wrong path — so
`out["classification_by_rater"]` above is still an error, `rater` being per-file rather than
per-run. Bind every placeholder a layout uses and the whole role list becomes walkable without
knowing which roles are special:

```python
out = bidslake.layout("feat").under(dst / stem, training="UKBiobank", threshold="1", rater="ab")
[out[role] for role in out.layout.roles]             # all 23, no special-casing
```

A layout is a separate artifact from the adapter's term map rather than that term map run
backwards, which [ADR 0002](../../docs/adr/0002-adapters-and-layouts.md) measures the cost of.
The two are still kept honest by construction: loading a layout renders every role under every
declared example and feeds the result back through the term map, and if `classify(render(role))`
does not reproduce the declared concepts it raises rather than loading. A layout that loads is
one whose roles agree with the term map that will later recognize their output.

That check has a consequence worth knowing before you reach for a role as a query filter. A
role's `Concepts`/`Entities` describe the file **at its destination**, not the file that will be
copied or computed into the slot. `feat`'s `highres` role declares `desc-brain`, which is what
FEAT's own copy is; the entity that would *select* an fMRIPrep T1w to fill that slot is
`desc-preproc`, and declaring that instead makes the role fail the round trip, by design.

## Design

- **Rust owns the connection.** The compiled extension (`bidslake._bidslake`,
  PyO3/maturin) opens the file with the bundled DuckDB engine and returns results
  as Arrow IPC — so there is **no `duckdb` Python dependency** and no engine
  version to keep in sync. Polars reads the Arrow.
- **No ORM.** Polars is the query builder; `get()` is a thin typed convenience
  layer over it.
- **Static-first typing.** `schema/_generated.py` (emitted from the Rust schema
  model, committed) provides `Literal`s for entities/datatypes/suffixes/
  modalities (and value-`Literal`s for `sex`/`handedness`), a `GetFilters`
  `TypedDict` for `get()`, and a `COLUMNS` map. Runtime `information_schema`
  validation is the backstop.

## Develop

Building the extension, running the suites, regenerating the typed schema module and what CI
checks are all in
[CONTRIBUTING.md](https://github.com/psadil/bidslake/blob/main/CONTRIBUTING.md).
