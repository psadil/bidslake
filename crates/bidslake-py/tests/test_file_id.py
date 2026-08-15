"""`file_id` as it reaches Python.

The registry key used to be a 128-bit `HUGEINT`, which the Arrow bridge hands over as
`Decimal128(38, 0)`. Those ranges do not match — `HUGEINT` reaches ~1.7e38 and
`Decimal(38,0)` stops at 1e38 — so **41% of the id space was outside the type the value
arrived in**. It read back fine and then failed the moment anyone built a new frame from
it:

    >>> pl.DataFrame([{"file_id": df["file_id"][0]}])
    RuntimeError: BindingsError: "Decimal is too large to fit in Decimal128"

Serializing a query result is an ordinary thing to do, and it failed for roughly two files
in five, chosen by hash — so it presented as flakiness rather than as a type error.
Widening the decimal was not available: polars caps precision at 38 because its `Decimal`
is Decimal128-backed, so the ceiling sits upstream of both crates.

`UBIGINT` crosses as `UInt64` and arrives as a plain `int`. These pin that.
"""

from __future__ import annotations

import polars as pl
from bidslake.schema.models import AllFiles
from sqlalchemy import select

#: Tables whose id column the round-trip has to hold for, not just `all_files`.
ID_TABLES = ("all_files", "file_registry", "sidecars")


def test_file_id_is_a_plain_integer(lake):
    df = lake.sql("SELECT file_id FROM all_files LIMIT 5")
    assert df.schema["file_id"] == pl.UInt64, f"expected UInt64, got {df.schema['file_id']}"
    assert all(isinstance(v, int) for v in df["file_id"])


def test_a_query_result_can_be_rebuilt_into_a_frame(lake):
    """The regression. This raised `Decimal is too large to fit in Decimal128`.

    What is pinned is that it *works* and that no value changes. The rebuilt dtype is
    polars' business, not bidslake's: from bare Python ints it infers a width that holds
    every value it was given (`Int128` here, since ids above `i64::MAX` are ordinary),
    which is correct and not something to assert against. Pass a schema if a caller wants
    `UInt64` back.
    """
    df = lake.sql("SELECT file_id, file_path FROM all_files")
    rows = df.rows(named=True)
    assert rows
    rebuilt = pl.DataFrame(rows)
    assert rebuilt.height == len(rows)
    assert rebuilt["file_id"].to_list() == df["file_id"].to_list(), "a value changed"
    # And with the width stated, it round-trips as itself.
    typed = pl.DataFrame(rows, schema_overrides={"file_id": pl.UInt64})
    assert typed.schema["file_id"] == pl.UInt64


def test_every_id_column_round_trips(lake):
    for table in ID_TABLES:
        df = lake.sql(f"SELECT file_id FROM {table} LIMIT 50")
        if df.is_empty():
            continue
        assert df.schema["file_id"] == pl.UInt64, table
        pl.DataFrame(df.rows(named=True))  # must not raise


def test_ids_are_never_negative(lake):
    """Under the old signed `HUGEINT` half of all ids were negative. Not any more."""
    n = lake.sql("SELECT count(*) AS n FROM all_files WHERE file_id < 0")["n"][0]
    assert n == 0


def test_an_id_binds_back_as_a_query_parameter(lake):
    """Round trip: take an id out, hand it back as a bind value, get the same row.

    A `file_id` is the natural thing to carry between a discovery query and a later one, so
    it has to survive the round trip as an ordinary Python value.
    """
    row = lake.sql("SELECT file_id, file_path FROM all_files LIMIT 1").row(0, named=True)
    back = lake.sql("SELECT file_path FROM all_files WHERE file_id = ?", [row["file_id"]])
    assert back.height == 1
    assert back["file_path"][0] == row["file_path"]


def test_an_id_binds_through_a_sqlalchemy_statement(lake):
    """The same, through the typed query layer rather than raw SQL."""
    row = lake.sql("SELECT file_id, file_path FROM all_files LIMIT 1").row(0, named=True)
    stmt = select(AllFiles.file_path).where(AllFiles.file_id == row["file_id"])
    assert lake.sql(stmt)["file_path"][0] == row["file_path"]


def test_the_id_is_stable_across_two_reads(lake):
    """It is a hash of (dataset_id, root_uri, file_path), so it cannot drift within a run."""
    a = lake.sql("SELECT file_id FROM all_files ORDER BY file_path")["file_id"].to_list()
    b = lake.sql("SELECT file_id FROM all_files ORDER BY file_path")["file_id"].to_list()
    assert a == b


def test_the_generated_types_agree_with_the_catalog(lake):
    """`COLUMNS` is emitted from the DDL; drift here means stale generated types."""
    from bidslake.schema import COLUMNS

    assert COLUMNS["all_files"]["file_id"] == "UBIGINT"
    assert lake.columns("file_registry")["file_id"] == "UBIGINT"
