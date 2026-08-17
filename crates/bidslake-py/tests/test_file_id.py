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
import pytest
from bidslake.schema.models import AllFiles
from sqlalchemy import select

#: Tables whose id column the round-trip has to hold for, not just `all_files`.
ID_TABLES = ("all_files", "file_registry", "sidecars")


@pytest.fixture(scope="module")
def registry_ids(lake):
    """A handful of ids straight off the registry."""
    return lake.sql("SELECT file_id FROM all_files LIMIT 5")


def test_file_id_crosses_as_unsigned_64_bit(registry_ids):
    assert registry_ids.schema["file_id"] == pl.UInt64


def test_file_id_arrives_as_a_plain_integer(registry_ids):
    """Not a `Decimal`: the width is polars', the Python type is what a caller handles."""
    assert all(isinstance(v, int) for v in registry_ids["file_id"])


# -- the regression: rebuilding a frame from a query result ------------------


@pytest.fixture(scope="module")
def id_rows(lake):
    """A full query result and its row dicts — the shape that used to raise."""
    df = lake.sql("SELECT file_id, file_path FROM all_files")
    rows = df.rows(named=True)
    assert rows, "fixture assumption: the catalog holds files"
    return df, rows


def test_a_query_result_can_be_rebuilt_into_a_frame(id_rows):
    """The regression. This raised `Decimal is too large to fit in Decimal128`.

    Compared value-by-value rather than merely counted, so the check covers both that the
    rebuild works and that nothing changed on the way. The rebuilt *dtype* is polars'
    business, not bidslake's: from bare Python ints it infers a width that holds every value
    it was given (`Int128` here, since ids above `i64::MAX` are ordinary), which is correct
    and not something to assert against.
    """
    df, rows = id_rows

    rebuilt = pl.DataFrame(rows)

    assert rebuilt["file_id"].to_list() == df["file_id"].to_list()


def test_a_stated_width_round_trips_as_itself(id_rows):
    """Pass a schema and `UInt64` comes back — the escape hatch for a caller who needs it."""
    _, rows = id_rows

    typed = pl.DataFrame(rows, schema_overrides={"file_id": pl.UInt64})

    assert typed.schema["file_id"] == pl.UInt64


# -- every table that carries an id, not just the registry -------------------


@pytest.fixture(params=ID_TABLES)
def id_column(request: pytest.FixtureRequest, lake):
    """One table's `file_id` column, as it crosses the Arrow bridge."""
    table = request.param
    df = lake.sql(f"SELECT file_id FROM {table} LIMIT 50")
    assert not df.is_empty(), f"fixture assumption: {table} should hold rows"
    return df


def test_every_id_column_crosses_as_unsigned_64_bit(id_column):
    assert id_column.schema["file_id"] == pl.UInt64


def test_every_id_column_rebuilds_into_a_frame(id_column):
    rebuilt = pl.DataFrame(id_column.rows(named=True))

    assert rebuilt["file_id"].to_list() == id_column["file_id"].to_list()


def test_ids_are_never_negative(lake):
    """Under the old signed `HUGEINT` half of all ids were negative. Not any more."""
    negative = lake.sql("SELECT count(*) AS n FROM all_files WHERE file_id < 0")

    assert negative["n"][0] == 0


# -- carrying an id between queries ------------------------------------------


@pytest.fixture(scope="module")
def one_file(lake):
    """An id and the path it belongs to — what a discovery query hands to a later one."""
    return lake.sql("SELECT file_id, file_path FROM all_files LIMIT 1").row(0, named=True)


def test_an_id_binds_back_as_a_query_parameter(lake, one_file):
    """A `file_id` has to survive the round trip as an ordinary Python value."""
    back = lake.sql("SELECT file_path FROM all_files WHERE file_id = ?", [one_file["file_id"]])

    assert back["file_path"].to_list() == [one_file["file_path"]]


def test_an_id_binds_through_a_sqlalchemy_statement(lake, one_file):
    """The same, through the typed query layer rather than raw SQL."""
    stmt = select(AllFiles.file_path).where(AllFiles.file_id == one_file["file_id"])

    assert lake.sql(stmt)["file_path"].to_list() == [one_file["file_path"]]


def test_the_id_is_stable_across_two_reads(lake):
    """It is a hash of (dataset_id, root_uri, file_path), so it cannot drift within a run.

    The two reads are the claim, so the pair of calls is one Act.
    """
    a = lake.sql("SELECT file_id FROM all_files ORDER BY file_path")["file_id"].to_list()
    b = lake.sql("SELECT file_id FROM all_files ORDER BY file_path")["file_id"].to_list()

    assert a == b


def test_the_catalog_stores_the_id_as_ubigint(lake):
    """The concrete type, which is the regression this file exists for.

    That the *generated* `COLUMNS` agrees with the catalog is `test_codegen`'s job, for
    every column of every table; asserting it again for this one would only duplicate it.
    """
    assert lake.columns("file_registry")["file_id"] == "UBIGINT"
