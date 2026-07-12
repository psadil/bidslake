"""Opening a database and inspecting its structure."""

from __future__ import annotations

import pytest


def test_tables_present(lake):
    expected = {"scans", "sidecars", "participants", "sessions", "events", "dataset_description"}
    assert expected <= set(lake.tables())


def test_scans_has_concept_columns(lake):
    concepts = {"sub", "ses", "task", "run", "datatype", "suffix", "extension", "modality"}
    assert concepts <= set(lake.columns("scans"))


def test_unknown_table_raises(lake):
    with pytest.raises(KeyError):
        lake.table("no_such_table")


def test_raw_sql_escape_hatch(lake):
    df = lake.sql(
        "SELECT count(*) AS n FROM scans WHERE datatype = ? AND suffix = ?",
        ["func", "bold"],
    )
    assert df["n"][0] > 0
