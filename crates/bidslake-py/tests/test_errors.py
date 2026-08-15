"""What a failed query tells you.

`PyLake` is the only way to run a statement, and `sibling()`-shaped queries are wide and
hand-composed, so a SQL error is the message a user meets most often. It used to be
`RuntimeError: preparing query` for every one of them -- the `.context()` replaced DuckDB's
diagnostic rather than adding to it, because `anyhow::Error`'s plain `Display` prints only
the outermost link.

These pin both halves of the fix: the engine's message survives, and the FFI leaf below it
(`Error code 1: Unknown error code`, appended to *everything* and never informative) does
not.
"""

from __future__ import annotations

import re

import bidslake
import pytest

#: The `duckdb::Error` source that carries no information. Never worth showing.
FFI_NOISE = "Unknown error code"


def test_missing_table_says_which_table(lake):
    with pytest.raises(RuntimeError) as exc:
        lake.sql("SELECT * FROM nosuchtable")
    msg = str(exc.value)
    assert "preparing query" in msg, "lost the context saying what bidslake was doing"
    assert "nosuchtable" in msg, "lost the engine's message naming the table"
    assert FFI_NOISE not in msg


def test_unknown_column_says_which_column(lake):
    with pytest.raises(RuntimeError) as exc:
        lake.sql("SELECT nosuchcolumn FROM all_files")
    msg = str(exc.value)
    assert "nosuchcolumn" in msg
    # DuckDB offers near-misses, which is most of the value of forwarding its text.
    assert "Binder Error" in msg
    assert FFI_NOISE not in msg


def test_syntax_error_says_where(lake):
    with pytest.raises(RuntimeError) as exc:
        lake.sql("SELECT * FROM all_files WHERE")
    msg = str(exc.value)
    assert "Parser Error" in msg
    assert FFI_NOISE not in msg


def test_three_different_mistakes_give_three_different_messages(lake):
    """The regression itself: these were once byte-identical."""
    msgs = set()
    for sql in (
        "SELECT * FROM nosuchtable",
        "SELECT nosuchcolumn FROM all_files",
        "SELECT * FROM all_files WHERE",
    ):
        with pytest.raises(RuntimeError) as exc:
            lake.sql(sql)
        msgs.add(str(exc.value))
    assert len(msgs) == 3


def test_open_failure_names_the_path(tmp_path):
    missing = tmp_path / "does-not-exist.duckdb"
    with pytest.raises(RuntimeError) as exc:
        bidslake.open(str(missing))
    msg = str(exc.value)
    assert str(missing) in msg
    assert "opening DuckDB database" in msg
    assert FFI_NOISE not in msg


def test_closed_handle_is_its_own_message(lake_db):
    lake = bidslake.open(str(lake_db))
    lake.close()
    with pytest.raises(RuntimeError, match=r"operation on closed BidsLake"):
        lake.sql("SELECT 1")


def test_table_accessor_validates_before_sql(lake):
    """`table()` never reaches DuckDB with a bad name, and says what is available.

    Worth pinning because it is the counter-example: the typed accessors were already
    good at this, which is part of why the raw-SQL path's silence went unnoticed.
    """
    with pytest.raises(KeyError) as exc:
        lake.table("nosuchtable")
    msg = str(exc.value)
    assert "nosuchtable" in msg
    assert "all_files" in msg, "the error should list what *is* available"


def test_no_message_ends_in_a_bare_error_code(lake):
    """Nothing should trail off into the FFI leaf, whatever the failure."""
    for sql in ("SELECT * FROM nosuchtable", "SELECT 1/0 AS x", "SELECT * FROM all_files WHERE"):
        try:
            lake.sql(sql)
        except RuntimeError as e:
            assert not re.search(r"Error code \d+", str(e)), f"FFI noise leaked for {sql!r}"
