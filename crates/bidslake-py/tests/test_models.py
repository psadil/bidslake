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


def test_a_keyword_column_is_reachable(lake) -> None:
    """`participant_id` is ordinary; the point of the model layer is that a column whose
    name is a Python keyword (fMRIPrep's `from`) still gets an attribute, suffixed."""
    stmt = select(Participants.participant_id).limit(1)
    assert lake.sql(stmt).height <= 1
