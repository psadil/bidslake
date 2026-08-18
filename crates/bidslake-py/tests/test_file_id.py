"""`file_id` as it reaches Python.

The registry key was briefly a 128-bit `HUGEINT`, which the Arrow bridge hands over as
`Decimal128(38, 0)`. Those ranges do not match — `HUGEINT` reaches ~1.7e38 and
`Decimal(38,0)` stops at 1e38 — so **41% of the id space was outside the type the value
arrived in**. It read back fine and then failed the moment anyone built a new frame from
it:

    >>> pl.DataFrame([{"file_id": df["file_id"][0]}])
    RuntimeError: BindingsError: "Decimal is too large to fit in Decimal128"

Serializing a query result is an ordinary thing to do, and it failed for roughly two files
in five, chosen by hash — so it presented as flakiness rather than as a type error. That
sent the key to a 64-bit `UBIGINT`, at the cost of collision resistance.

It is 128 bits again, and the thing that changed is not the width but the *type*: the
blocker was never width, it was `HUGEINT`'s Arrow mapping. A DuckDB `UUID` is equally 128
bits, is `PhysicalType::INT128` inside the engine — so it keeps the integer join width and
compression an id column wants — and crosses Arrow as `Utf8`. It arrives as a `str`.

Python therefore sees **one** spelling of an id, everywhere: the canonical string. That is
not a preference, it is forced — polars has no UUID dtype, and the only route to 16 raw
bytes is `arrow_lossless_conversion`, which is connection-wide and also retypes every
`BOOLEAN` column. So a frame column is `pl.String`, `BidsFile.file_id` is a `str`, and
`bidslake.file_id(...)` returns a `str`, and any two of them compare directly. A
`uuid.UUID` is still *accepted* as a bind parameter, for a caller who has built one.
"""

from __future__ import annotations

import uuid

import bidslake
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


def test_file_id_crosses_as_a_string(registry_ids):
    """`UUID` exports as Arrow `Utf8`; polars has no UUID dtype to receive it as."""
    assert registry_ids.schema["file_id"] == pl.String


def test_file_id_arrives_as_a_well_formed_v8_uuid(registry_ids):
    """Not merely 32 hex digits: the version and variant nibbles are stamped.

    A raw SHA-256 prefix would parse as a UUID and report whichever version the hash
    happened to land on — `uuid.UUID(...).version` is confident and wrong. Stamping RFC
    9562 v8 (the vendor-specific version) is what makes the answer honest, and costs 6 of
    the 128 bits.
    """
    ids = [uuid.UUID(v) for v in registry_ids["file_id"]]

    assert all(i.version == 8 for i in ids)
    assert all(str(i) == v for i, v in zip(ids, registry_ids["file_id"], strict=True)), (
        "the stored spelling is already canonical, so it survives a parse unchanged"
    )


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
    rebuild works and that nothing changed on the way. A string has no width to overflow,
    which is the whole reason this shape is safe now rather than merely narrow enough.
    """
    df, rows = id_rows

    rebuilt = pl.DataFrame(rows)

    assert rebuilt["file_id"].to_list() == df["file_id"].to_list()
    assert rebuilt.schema["file_id"] == pl.String


# -- every table that carries an id, not just the registry -------------------


@pytest.fixture(params=ID_TABLES)
def id_column(request: pytest.FixtureRequest, lake):
    """One table's `file_id` column, as it crosses the Arrow bridge."""
    table = request.param
    df = lake.sql(f"SELECT file_id FROM {table} LIMIT 50")
    assert not df.is_empty(), f"fixture assumption: {table} should hold rows"
    return df


def test_every_id_column_crosses_as_a_string(id_column):
    assert id_column.schema["file_id"] == pl.String


def test_every_id_column_rebuilds_into_a_frame(id_column):
    rebuilt = pl.DataFrame(id_column.rows(named=True))

    assert rebuilt["file_id"].to_list() == id_column["file_id"].to_list()


# -- carrying an id between queries ------------------------------------------


@pytest.fixture(scope="module")
def one_file(lake):
    """An id and the path it belongs to — what a discovery query hands to a later one."""
    return lake.sql("SELECT file_id, file_path FROM all_files LIMIT 1").row(0, named=True)


def test_an_id_binds_back_as_a_query_parameter(lake, one_file):
    """A `file_id` has to survive the round trip as an ordinary Python value."""
    back = lake.sql("SELECT file_path FROM all_files WHERE file_id = ?", [one_file["file_id"]])

    assert back["file_path"].to_list() == [one_file["file_path"]]


def test_a_uuid_object_binds_back_too(lake, one_file):
    """The other spelling: what `BidsFile.file_id` hands you, without a `str()` in between.

    Without the `uuid::Uuid` arm in `py_to_duck_value` this fell past every arm to a
    `TypeError`, so the two spellings of the same value did not behave the same.
    """
    back = lake.sql(
        "SELECT file_path FROM all_files WHERE file_id = ?",
        [uuid.UUID(one_file["file_id"])],
    )

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


def test_the_catalog_stores_the_id_as_uuid(lake):
    """The concrete type, which is the regression this file exists for.

    That the *generated* `COLUMNS` agrees with the catalog is `test_codegen`'s job, for
    every column of every table; asserting it again for this one would only duplicate it.
    """
    assert lake.columns("file_registry")["file_id"] == "UUID"


# -- the derivation, exported ------------------------------------------------


def test_a_bids_file_spells_its_id_the_same_way_a_frame_does(lake):
    """The whole point of one spelling: no conversion between the two ways in.

    This is the regression guard for a boundary that has been wrong before. When the two
    disagreed, nothing caught it statically — polars is not generic over its schema, so a
    frame element is `Any` and a type checker sees no conflict. It failed at runtime, in
    user code, as a comparison that was quietly always false.
    """
    f = next(iter(lake.get()))
    from_frame = lake.sql(
        "SELECT file_id FROM all_files WHERE file_path = ? AND root_uri = ?",
        [f.file_path, f.root_uri],
    )["file_id"][0]

    assert isinstance(f.file_id, str)
    assert f.file_id == from_frame


def test_the_derivation_is_reproducible_without_a_catalog(lake):
    r"""`bidslake.file_id(...)` is the supported way to compute an id by hand.

    The point is that a caller can check or construct one without a query — and without
    reimplementing SHA-256 and the `\\x1f` join from a Rust doc comment, which was the only
    option before.
    """
    f = next(iter(lake.get()))

    assert bidslake.file_id(f.dataset_id, f.root_uri, f.file_path) == f.file_id


def test_the_root_is_part_of_the_identity():
    """A dataset may span several roots (docs/adr/0005), so the path alone is not the file."""
    a = bidslake.file_id("ds", "file:///r1", "desc-aseg_dseg.tsv")
    b = bidslake.file_id("ds", "file:///r2", "desc-aseg_dseg.tsv")

    assert a != b
