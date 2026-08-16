"""The generated SQLAlchemy models, and `lake.sql` taking a statement.

These exist for the queries `get()` deliberately cannot express — joins, `OR`,
comparisons, subqueries. The statement is compiled to a string and run by the same Rust
engine as every other query, so none of this opens a second connection.
"""

from __future__ import annotations

import pytest
from bidslake._sql import compile_statement
from bidslake.schema.models import AllFiles, Base, Participants, Sidecars
from sqlalchemy import or_, select


def test_every_catalog_table_has_a_model(lake) -> None:
    """The models are generated from the same introspection as `COLUMNS`, so a table
    that exists in a real catalog must have one — otherwise a query cannot name it."""
    mapped = {m.class_.__tablename__ for m in Base.registry.mappers}

    missing = sorted(set(lake.tables()) - mapped)

    assert missing == []


@pytest.fixture(scope="module")
def bold_paths_two_ways(lake):
    """The same query written as a statement and as text."""
    stmt = select(AllFiles.file_path).where(AllFiles.suffix == "bold", AllFiles.kind == "data")
    from_stmt = lake.sql(stmt)
    from_text = lake.sql(
        "SELECT file_path FROM all_files WHERE suffix = ? AND kind = ?", ["bold", "data"]
    )
    assert from_stmt.height > 0, "fixture assumption: the catalog holds BOLD data files"
    return from_stmt, from_text


def test_a_statement_runs_through_the_rust_engine(bold_paths_two_ways) -> None:
    """`lake.sql(select(...))` is the same path as `lake.sql("...")`, not a second one.
    That the two agree is the claim, so the pair of calls is one Act."""
    from_stmt, from_text = bold_paths_two_ways

    assert from_stmt.equals(from_text)


@pytest.fixture(scope="module")
def join_result(lake):
    """The reason this surface exists: `get()` is one table and conjunction-only."""
    stmt = (
        select(AllFiles.file_path, Sidecars.RepetitionTime)
        .join(Sidecars, Sidecars.file_id == AllFiles.file_id)
        .where(AllFiles.suffix == "bold", AllFiles.kind == "data")
    )
    return lake.sql(stmt)


def test_a_join_selects_from_both_tables(join_result) -> None:
    assert join_result.columns == ["file_path", "RepetitionTime"]


def test_a_join_returns_rows(join_result) -> None:
    assert join_result.height > 0


def test_or_and_comparison(lake) -> None:
    """Neither is expressible in `get()`'s filter language, which is `AND` of equalities.
    The three calls make one claim — the disjunction is exactly the two branches."""
    stmt = select(AllFiles.file_path).where(
        AllFiles.kind == "data",
        or_(AllFiles.suffix == "bold", AllFiles.suffix == "T1w"),
    )

    both = lake.sql(stmt).height
    bold = len(list(lake.get(suffix="bold", kind="data")))
    t1w = len(list(lake.get(suffix="T1w", kind="data")))

    assert both == bold + t1w


def test_a_generated_model_reaches_the_catalog(lake) -> None:
    """A model attribute names a real column, so a `select` over it runs.

    The keyword-column case the model layer also handles — a column named `from`, which
    cannot be an attribute and is emitted as `from_` — is not reachable from here: the
    bundled schema declares no such column, so it is pinned in `test_overlay.py`, against a
    catalog whose fMRIPrep overlay does.
    """
    rows = lake.sql(select(Participants.participant_id))

    assert rows.height > 0


# -- compilation, which happens without a database ---------------------------


@pytest.fixture(scope="module")
def three_equalities():
    """A statement with three bound values, compiled once."""
    stmt = select(AllFiles.file_path).where(
        AllFiles.sub == "SUBJ", AllFiles.task == "TASK", AllFiles.suffix == "SUFFIX"
    )
    return compile_statement(stmt)


def test_one_placeholder_per_bound_value(three_equalities) -> None:
    sql, _ = three_equalities

    assert sql.count("?") == 3


def test_params_follow_the_placeholders_not_dict_order(three_equalities) -> None:
    """A compiled statement's params come back as a dict; pairing them in any order but
    `positiontup` binds the wrong value to the wrong `?`, silently."""
    _, params = three_equalities

    assert params == ["SUBJ", "TASK", "SUFFIX"]


def test_values_are_never_interpolated_into_the_sql(three_equalities) -> None:
    sql, _ = three_equalities

    assert "SUBJ" not in sql


@pytest.fixture(scope="module")
def compiled_in_clause():
    """`in_()` builds an *expanding* param that SQLAlchemy only expands when it executes
    the statement — which never happens here. Unexpanded, the SQL keeps a literal
    `__[POSTCOMPILE_x]` token and the param stays one nested list, so there is nothing for
    DuckDB to bind. No `lake` fixture: this needs no database, and the guard should still
    run where the bids-examples submodule is missing."""
    return compile_statement(select(AllFiles.file_path).where(AllFiles.suffix.in_(["bold", "T1w"])))


def test_an_expanding_param_is_expanded(compiled_in_clause) -> None:
    sql, _ = compiled_in_clause

    assert "__[POSTCOMPILE" not in sql


def test_an_in_clause_gets_one_placeholder_per_value(compiled_in_clause) -> None:
    sql, _ = compiled_in_clause

    assert sql.count("?") == 2


def test_an_in_clause_binds_a_flat_list(compiled_in_clause) -> None:
    _, params = compiled_in_clause

    assert params == ["bold", "T1w"]


@pytest.fixture(scope="module")
def compiled_empty_in_clause():
    """The empty sequence takes a separate branch: SQLAlchemy renders `IN (NULL) AND
    (1 != 1)` rather than an empty `IN ()`, which is not valid SQL. Matching nothing is
    also what `get()` means by an empty sequence (`_compile_filters` emits `FALSE`), so
    both spellings of the query agree."""
    return compile_statement(select(AllFiles.file_path).where(AllFiles.suffix.in_([])))


def test_an_empty_expanding_param_is_expanded(compiled_empty_in_clause) -> None:
    sql, _ = compiled_empty_in_clause

    assert "__[POSTCOMPILE" not in sql


def test_an_empty_in_clause_binds_nothing(compiled_empty_in_clause) -> None:
    sql, params = compiled_empty_in_clause

    assert (sql.count("?"), params) == (0, [])


def test_in_clause_matches_the_same_rows_as_get(lake) -> None:
    """The compiled form has to survive the round trip, not just look right: `get()`'s
    hand-rolled `IN` is the reference, and the statement must select the same files.
    Compared by `file_id` because `file_path` is dataset-relative and collides across the
    fixture's datasets, where a set comparison would quietly dedupe the difference away.

    An empty result would prove nothing, so it is ruled out rather than asserted around.
    """
    stmt = select(AllFiles.file_id).where(AllFiles.suffix.in_(["bold", "T1w"]))

    from_sql = sorted(lake.sql(stmt)["file_id"])
    from_get = sorted(f.file_id for f in lake.get(suffix=["bold", "T1w"]))

    assert from_sql == from_get != []
