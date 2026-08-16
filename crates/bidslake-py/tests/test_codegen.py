"""The generated schema types must stay in lockstep with the real database.

This covers only the DB-introspected part: the committed `COLUMNS` (emitted from
the Rust schema model by `emit-types`) must equal what an actual ingested database
contains. The schema-JSON value-set `Literal`s (Entity/Datatype/Suffix/Modality/
Sex/Handedness) are NOT checked here — the `codegen-drift` CI job
(.github/workflows/ci.yml) re-runs `emit-types` and `git diff --exit-code`s
`_generated.py`, which is what actually guards those.

The per-table checks are parametrized rather than looped so that a drifted table names
itself in the failure, and so one drifted table does not hide the next.
"""

from __future__ import annotations

import polars as pl
import pytest
from bidslake.schema import COLUMNS, C

TABLES = sorted(COLUMNS)


def test_generated_tables_match_database(lake):
    assert set(COLUMNS) == set(lake.tables())


@pytest.mark.parametrize(
    ("accessor", "column"),
    [(C.all_files.task, "task"), (C.sidecars.RepetitionTime, "RepetitionTime")],
)
def test_c_namespace_is_typed_pl_col(accessor, column):
    """Per-table accessors resolve to the matching pl.col expression."""
    assert str(accessor) == str(pl.col(column))


@pytest.mark.parametrize("table", TABLES)
def test_generated_columns_match_database(lake, table):
    assert set(COLUMNS[table]) == set(lake.columns(table))


@pytest.mark.parametrize("table", TABLES)
def test_generated_column_types_match_database(lake, table):
    actual = lake.columns(table)
    mismatches = {
        name: (dtype, actual.get(name))
        for name, dtype in COLUMNS[table].items()
        if actual.get(name) != dtype
    }

    assert mismatches == {}
