"""The generated schema types must stay in lockstep with the real database.

This is the drift guard: the committed `COLUMNS` (emitted from the Rust schema
model by `emit-types`) must equal what an actual ingested database contains.
Paired in CI with a `git diff --exit-code` after re-running `emit-types`.
"""

from __future__ import annotations

import polars as pl
from bidslake.schema import COLUMNS, C


def test_generated_tables_match_database(lake):
    assert set(COLUMNS) == set(lake.tables())


def test_c_namespace_is_typed_pl_col():
    # Per-table accessors resolve to the matching pl.col expression.
    assert str(C.scans.task) == str(pl.col("task")) and str(C.sidecars.RepetitionTime) == str(
        pl.col("RepetitionTime")
    )


def test_generated_columns_match_database(lake):
    mismatches = {table: set(cols) ^ set(lake.columns(table)) for table, cols in COLUMNS.items()}
    assert not any(mismatches.values())


def test_generated_column_types_match_database(lake):
    mismatches = {
        f"{table}.{name}": (dtype, lake.columns(table).get(name))
        for table, cols in COLUMNS.items()
        for name, dtype in cols.items()
        if lake.columns(table).get(name) != dtype
    }
    assert not mismatches
