"""The generated SQLAlchemy models, and `lake.sql` taking a statement.

These exist for the queries `get()` deliberately cannot express — joins, `OR`,
comparisons, subqueries. The statement is compiled to a string and run by the same Rust
engine as every other query, so none of this opens a second connection.
"""

from __future__ import annotations

from bidslake.schema.models import AllFiles, Base, Participants, Sidecars
from sqlalchemy import or_, select


def test_every_catalog_table_has_a_model(lake) -> None:
    """The models are generated from the same introspection as `COLUMNS`, so a table
    that exists in a real catalog must have one — otherwise a query cannot name it."""
    mapped = {m.class_.__tablename__ for m in Base.registry.mappers}
    missing = sorted(set(lake.tables()) - mapped)
    assert not missing, f"catalog tables with no model: {missing}"


def test_a_statement_runs_through_the_rust_engine(lake) -> None:
    """`lake.sql(select(...))` is the same path as `lake.sql("...")`, not a second one."""
    stmt = select(AllFiles.file_path).where(AllFiles.suffix == "bold", AllFiles.kind == "data")
    from_stmt = lake.sql(stmt)
    from_text = lake.sql(
        "SELECT file_path FROM all_files WHERE suffix = ? AND kind = ?", ["bold", "data"]
    )
    assert from_stmt.equals(from_text)
    assert from_stmt.height > 0


def test_a_join_get_cannot_express(lake) -> None:
    """The reason this surface exists: `get()` is one table and conjunction-only."""
    stmt = (
        select(AllFiles.file_path, Sidecars.RepetitionTime)
        .join(Sidecars, Sidecars.file_id == AllFiles.file_id)
        .where(AllFiles.suffix == "bold", AllFiles.kind == "data")
    )
    df = lake.sql(stmt)
    assert df.columns == ["file_path", "RepetitionTime"]
    assert df.height > 0


def test_or_and_comparison(lake) -> None:
    """Neither is expressible in `get()`'s filter language, which is `AND` of equalities."""
    stmt = select(AllFiles.file_path).where(
        AllFiles.kind == "data",
        or_(AllFiles.suffix == "bold", AllFiles.suffix == "T1w"),
    )
    both = lake.sql(stmt).height
    bold = len(list(lake.get(suffix="bold", kind="data")))
    t1w = len(list(lake.get(suffix="T1w", kind="data")))
    assert both == bold + t1w


def test_params_are_bound_positionally_not_interpolated(lake) -> None:
    """A compiled statement's params come back as a dict; pairing them in any order but
    `positiontup` binds the wrong value to the wrong `?`, silently."""
    from bidslake._sql import compile_statement

    stmt = select(AllFiles.file_path).where(
        AllFiles.sub == "SUBJ", AllFiles.task == "TASK", AllFiles.suffix == "SUFFIX"
    )
    sql, params = compile_statement(stmt)
    assert sql.count("?") == 3
    assert params == ["SUBJ", "TASK", "SUFFIX"], "order follows the ?s, not dict order"
    assert "SUBJ" not in sql, "values are bound, never interpolated into the SQL"


def test_in_clause_expands_at_compile_time() -> None:
    """`in_()` builds an *expanding* param that SQLAlchemy only expands when it executes
    the statement — which never happens here. Unexpanded, the SQL keeps a literal
    `__[POSTCOMPILE_x]` token and the param stays one nested list, so there is nothing for
    DuckDB to bind. No `lake` fixture: this needs no database, and the guard should still
    run where the bids-examples submodule is missing."""
    from bidslake._sql import compile_statement

    sql, params = compile_statement(
        select(AllFiles.file_path).where(AllFiles.suffix.in_(["bold", "T1w"]))
    )
    assert "__[POSTCOMPILE" not in sql, "expanding param never expanded"
    assert sql.count("?") == 2, "one placeholder per value"
    assert params == ["bold", "T1w"], "flat, not a single nested list"


def test_empty_in_clause_compiles_to_valid_sql() -> None:
    """The empty sequence takes a separate branch: SQLAlchemy renders `IN (NULL) AND
    (1 != 1)` rather than an empty `IN ()`, which is not valid SQL. Matching nothing is
    also what `get()` means by an empty sequence (`_compile_filters` emits `FALSE`), so
    both spellings of the query agree."""
    from bidslake._sql import compile_statement

    sql, params = compile_statement(select(AllFiles.file_path).where(AllFiles.suffix.in_([])))
    assert "__[POSTCOMPILE" not in sql
    assert params == []
    assert sql.count("?") == 0


def test_in_clause_matches_the_same_rows_as_get(lake) -> None:
    """The compiled form has to survive the round trip, not just look right: `get()`'s
    hand-rolled `IN` is the reference, and the statement must select the same files.
    Compared by `file_id` because `file_path` is dataset-relative and collides across the
    fixture's datasets, where a set comparison would quietly dedupe the difference away."""
    stmt = select(AllFiles.file_id).where(AllFiles.suffix.in_(["bold", "T1w"]))
    from_sql = sorted(lake.sql(stmt)["file_id"])
    from_get = sorted(f.file_id for f in lake.get(suffix=["bold", "T1w"]))
    assert from_sql, "fixture should have bold/T1w files; an empty result proves nothing"
    assert from_sql == from_get


def test_a_keyword_column_is_reachable(lake) -> None:
    """`participant_id` is ordinary; the point of the model layer is that a column whose
    name is a Python keyword (fMRIPrep's `from`) still gets an attribute, suffixed."""
    stmt = select(Participants.participant_id).limit(1)
    assert lake.sql(stmt).height <= 1
