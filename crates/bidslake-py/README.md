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
