"""The raw-SQL escape hatch, including the t-string form."""

from __future__ import annotations


def test_sql_string_with_params(lake):
    df = lake.sql("SELECT count(*) AS n FROM scans WHERE suffix = ?", ["bold"])
    assert df["n"][0] > 0


def test_sql_tstring_binds_interpolations(lake):
    suffix = "bold"
    task = "rest"
    df = lake.sql(t"SELECT count(*) AS n FROM scans WHERE suffix = {suffix} AND task = {task}")
    assert df["n"][0] > 0


def test_sql_tstring_is_injection_safe(lake):
    # A value that would be catastrophic if concatenated is bound as a literal,
    # so it matches nothing and scans is left intact.
    evil = "bold'; DROP TABLE scans; --"
    matched = lake.sql(t"SELECT count(*) AS n FROM scans WHERE suffix = {evil}")["n"][0]
    still_there = lake.sql("SELECT count(*) AS n FROM scans")["n"][0]
    assert (matched, still_there > 0) == (0, True)
