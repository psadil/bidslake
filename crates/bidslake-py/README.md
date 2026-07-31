# bidslake (Python)

Typed querying of [bidslake](../bidslake) datasets — the BIDSLayout / bids2table
analog. Open the DuckDB catalog a `bidslake index` produced and query it by BIDS
concept, getting back Polars or an iterable of file handles.

```python
import bidslake

lake = bidslake.open("study.duckdb")

# The headline: an iterable of every resting-state fMRI file, across all
# datasets in the catalog.
for f in lake.get(task="rest", suffix="bold", extension=".nii.gz"):
    img = nib.load(f.local_path)     # resolved from root_uri
    tr = f.metadata["RepetitionTime"]  # sidecar metadata, BIDS-cased
    events = f.get_events()          # associated events (inheritance-resolved)

# Whole tables as Polars (eager or lazy with projection pushdown):
df = lake.scans.pl()
lf = lake.sidecars.lazy().select("dataset_id", "RepetitionTime")

# Typed per-table column expressions and the wide one-big-table view:
from bidslake import C
lake.scans.pl().filter((C.scans.task == "rest") & (C.scans.suffix == "bold"))
lake.files.pl()                      # scans + sidecar__*/participant__*/dataset__*

# Safe raw SQL via t-strings:
lake.sql(t"SELECT count(*) FROM scans WHERE suffix = {suffix}")
```

## Reading what the catalog did not store

A catalog holds what its schema declares it holds. A table whose ingestion policy is
`undeclared: catalog` keeps only its declared columns, and the file on disk stays the
record of the rest — which is how fMRIPrep confounds stay tractable, since a single
file has ~1,800 columns against the ~13 the schema names (see
[ADR 0004](../../docs/adr/0004-undeclared-column-policy.md)).

Nothing is unreachable. `tabular_files` indexes every tabular file the ingest saw, and
`lake.resolve()` opens any of them:

```python
import polars as pl

# What names exist but are not stored — no disk access.
lake.table("tabular_undeclared_columns").pl()

# And the values, from the file itself.
row = (lake.table("tabular_files").pl()
       .filter(pl.col("table_name") == "fmriprep_confounds")
       .row(0, named=True))
path = lake.resolve(row["dataset_id"], row["file_path"])
full = pl.read_csv(path.open("rb"), separator="\t", null_values="n/a")
full.select("^a_comp_cor_.*$")
```

`resolve()` uses the same root resolution as `BidsFile.path` (so `base_dir` and
`root_override` apply) and returns a `UPath`. Read through `.open()` rather than
`str(path)` — a `UPath` stringifies back to a URI, and `.open()` is what keeps this
working for a remote dataset.

To store one of those columns instead, declare it in the overlay: it then gets a typed
column and costs 8 bytes a row. Declared-ness is the dial.

## Bindings: units of work

A pipeline step rarely operates on one file. It operates on a *unit* — a subject, a
session, a run — and needs several sibling files that belong to it, each matched on a
**different subset** of its entities. Written by hand that is one query per sibling per
unit, each wrapped in a "there must be exactly one" check that raises; a subject missing
one input then aborts the loop at whatever hour it is reached.

```python
from bidslake import Binding, FileInput, TableInput

DENOISE = Binding(
    anchor={"datatype": "func", "suffix": "bold", "desc": "preproc",
            "extension": ".nii.gz", "space": None},
    key=("sub", "ses", "task", "run"),
    inputs={
        "brain": FileInput(join=("sub", "ses", "task", "run"),
                           where={"datatype": "func", "suffix": "mask",
                                  "desc": "brain", "extension": ".nii.gz", "space": None}),
        # one T1w per session, so this joins on a *subset* of the key
        "anat":  FileInput(join=("sub", "ses"),
                           where={"datatype": "anat", "suffix": "T1w",
                                  "desc": "preproc", "extension": ".nii.gz", "space": None}),
        # a different dataset in the same catalog
        "wmparc": FileInput(join=("sub", "ses"), dataset_id="freesurfer",
                            where={"seg": "wmparc", "extension": ".mgz"}),
        # not a file at all: six columns of an ingested table, in row order
        "motion": TableInput(join=("sub", "ses", "task", "run"),
                             table="fmriprep_confounds", order_by="row_idx",
                             columns=("rot_x", "rot_y", "rot_z",
                                      "trans_x", "trans_y", "trans_z")),
    },
)

for unit in lake.bind(DENOISE):
    if unit.unresolved:
        print(unit.key, unit.unresolved)   # data, not an exception
        continue
    work(unit.anchor.local_path, unit.inputs["anat"], unit.inputs["motion"])
```

Two properties are the point. Resolution costs **one query per input**, not one per input
per unit, so it does not scale with the study. And a unit whose inputs do not resolve is
*returned*, carrying `Unresolved(name, n_matched, reason)` entries that separate *missing*
(the unit is incomplete) from *ambiguous* (the binding under-specifies — e.g. joining on
`sub` alone across datasets whose subject labels collide). Incomplete subjects are visible
before anything is submitted.

A binding is only a query; bidslake schedules nothing. The same declaration drives a
`for` loop, a process pool, a SLURM array, or a Snakemake input function:

```python
import submitit

units = [u for u in lake.bind(DENOISE) if not u.unresolved]
executor = submitit.AutoExecutor(folder="logs")
executor.update_parameters(timeout_min=240, slurm_partition="normal", cpus_per_task=4)
jobs = executor.map_array(run_one_unit, units)   # one job per unit
```

`bidslake.binding` is deliberately typed Python rather than a stamped JSON artifact for
now — the dataclasses match what such an artifact would hold, so promoting it later is a
serializer rather than a redesign. The module docstring and `TODO.md` ("Derivation layer")
record the reasoning and the trigger.

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

Requires the Rust toolchain and [`uv`](https://docs.astral.sh/uv/).

```bash
uv venv --python 3.14           # Python 3.14 floor (t-strings, Unpack, `type`)
uv pip install maturin
.venv/bin/maturin develop       # build + install the extension (editable)
.venv/bin/python -m pytest      # run tests (ingests bids-examples via `cargo index`)
.venv/bin/ty check python/bidslake
```

Set `BIDSLAKE_TEST_DB=/path/to.duckdb` to reuse a prebuilt database and skip the
(slow) per-session ingest.

### Regenerating the typed schema module

`schema/_generated.py` is committed and produced by a Rust bin that reuses the
exact `bidslake` schema/DDL model (no logic is re-implemented in Python):

```bash
# PYO3_PYTHON points cargo's link step at the venv interpreter.
PYO3_PYTHON=$PWD/.venv/bin/python cargo run -p bidslake-py --bin emit-types
```

CI (`.github/workflows/ci.yml`, also runnable locally):

- `pytest` — includes `test_codegen.py` (generated `COLUMNS` == the real
  database) and `test_typing.py` (asserts `ty` *rejects* a fixture of bad
  queries — the one typing check the `ty` hook can't make).
- `codegen-drift` job — re-runs `emit-types` and `git diff --exit-code` on
  `_generated.py`; fails if the committed types drifted from the schema. This is
  the only check that covers the value-set `Literal`s (Datatype/Suffix/Modality/…),
  which `test_codegen.py` (DB-introspected `COLUMNS` only) does not.
- `ty check python/bidslake`.
