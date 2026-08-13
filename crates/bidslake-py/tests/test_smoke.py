"""Opening a database and inspecting its structure."""

from __future__ import annotations

import pytest


def test_tables_present(lake):
    expected = {
        "file_registry",
        "all_files",
        "dataset_roots",
        "scans",
        "sidecars",
        "participants",
        "sessions",
        "events",
        "dataset_description",
    }
    assert expected <= set(lake.tables())


def test_registry_has_concept_columns(lake):
    # The concepts live once, on the registry view; the satellites key on `file_id`.
    concepts = {"sub", "ses", "task", "run", "datatype", "suffix", "extension", "modality"}
    assert concepts <= set(lake.columns("all_files"))
    assert set(lake.columns("scans")).isdisjoint(concepts)


def test_a_satellite_is_queried_by_concept_anyway(lake):
    # `get` joins a file-keyed table back to the registry, so a concept filter reaches
    # a table that stores no concepts at all (docs/adr/0006). `sidecars` rather than
    # `scans`: the fixtures ship no `scans.tsv`, and `scans` is that file's satellite.
    assert not {"suffix", "task"} & set(lake.columns("sidecars"))
    assert list(lake.get(table="sidecars", suffix="bold"))


def test_unknown_table_raises(lake):
    with pytest.raises(KeyError):
        lake.table("no_such_table")


def test_raw_sql_escape_hatch(lake):
    df = lake.sql(
        "SELECT count(*) AS n FROM all_files WHERE datatype = ? AND suffix = ?",
        ["func", "bold"],
    )
    assert df["n"][0] > 0
