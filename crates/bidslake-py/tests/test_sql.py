"""The raw-SQL escape hatch, including the t-string form."""

from __future__ import annotations

import pytest

#: A value that would drop the registry if it were concatenated into the statement text
#: rather than bound.
INJECTION = "bold'; DROP TABLE file_registry; --"


def test_sql_string_with_params(lake):
    df = lake.sql("SELECT count(*) AS n FROM all_files WHERE suffix = ?", ["bold"])

    assert df["n"][0] > 0


def test_sql_tstring_binds_interpolations(lake):
    suffix = "bold"
    task = "rest"

    df = lake.sql(t"SELECT count(*) AS n FROM all_files WHERE suffix = {suffix} AND task = {task}")

    assert df["n"][0] > 0


def test_an_injected_value_matches_nothing(lake):
    """Bound as a literal, so it is a suffix no file has rather than the end of the statement."""
    df = lake.sql(t"SELECT count(*) AS n FROM all_files WHERE suffix = {INJECTION}")

    assert df["n"][0] == 0


@pytest.fixture
def after_an_injection_attempt(lake):
    """The injecting value has already been through `sql` once."""
    lake.sql(t"SELECT count(*) AS n FROM all_files WHERE suffix = {INJECTION}")
    return lake


def test_the_registry_survives_an_injected_value(after_an_injection_attempt):
    """The other half: matching nothing would be small comfort if the `DROP` had run."""
    df = after_an_injection_attempt.sql("SELECT count(*) AS n FROM file_registry")

    assert df["n"][0] > 0
