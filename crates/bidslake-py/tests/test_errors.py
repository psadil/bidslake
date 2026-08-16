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

#: The `duckdb::Error` source that carries no information. Never worth showing. Matched as a
#: pattern rather than a literal so *any* bare code is caught, not just the one seen so far.
FFI_LEAF = re.compile(r"Error code \d+")

#: One mistake per case, with the fragments its message must carry. Each fragment is
#: asserted on its own so a message that loses one still reports what it kept.
BAD_SQL = {
    # The context saying what bidslake was doing, *and* the engine's message naming the table.
    "missing table": ("SELECT * FROM nosuchtable", ["preparing query", "nosuchtable"]),
    # DuckDB offers near-misses, which is most of the value of forwarding its text.
    "unknown column": ("SELECT nosuchcolumn FROM all_files", ["nosuchcolumn", "Binder Error"]),
    "syntax": ("SELECT * FROM all_files WHERE", ["Parser Error"]),
}

MESSAGE_FRAGMENTS = [(case, frag) for case, (_, frags) in BAD_SQL.items() for frag in frags]


@pytest.fixture(scope="module")
def sql_error_messages(lake) -> dict[str, str]:
    """The message each mistake raises.

    `pytest.raises` here rather than in the tests is what keeps them honest: a statement
    that stopped raising fails the whole family loudly, instead of leaving assertions that
    silently never run.
    """
    messages = {}
    for case, (sql, _) in BAD_SQL.items():
        with pytest.raises(RuntimeError) as exc:
            lake.sql(sql)
        messages[case] = str(exc.value)
    return messages


@pytest.mark.parametrize(("case", "fragment"), MESSAGE_FRAGMENTS)
def test_the_message_carries_the_engines_diagnostic(sql_error_messages, case, fragment):
    assert fragment in sql_error_messages[case]


@pytest.mark.parametrize("case", list(BAD_SQL))
def test_no_message_ends_in_a_bare_error_code(sql_error_messages, case):
    """Nothing should trail off into the FFI leaf, whatever the failure."""
    assert not FFI_LEAF.search(sql_error_messages[case])


def test_three_different_mistakes_give_three_different_messages(sql_error_messages):
    """The regression itself: these were once byte-identical."""
    assert len(set(sql_error_messages.values())) == len(BAD_SQL)


# -- failures that are not a query -------------------------------------------


@pytest.fixture
def open_failure(tmp_path) -> tuple[str, str]:
    """The path asked for, and the message `open` raised for it."""
    missing = tmp_path / "does-not-exist.duckdb"
    with pytest.raises(RuntimeError) as exc:
        bidslake.open(str(missing))
    return str(missing), str(exc.value)


def test_open_failure_names_the_path(open_failure):
    path, message = open_failure

    assert path in message


def test_open_failure_says_what_it_was_doing(open_failure):
    _, message = open_failure

    assert "opening DuckDB database" in message


def test_open_failure_ends_in_no_bare_error_code(open_failure):
    _, message = open_failure

    assert not FFI_LEAF.search(message)


def test_closed_handle_is_its_own_message(lake_db):
    lake = bidslake.open(str(lake_db))
    lake.close()

    with pytest.raises(RuntimeError, match=r"operation on closed BidsLake"):
        lake.sql("SELECT 1")


# -- the typed accessor, which was already good at this ----------------------


@pytest.fixture(scope="module")
def unknown_table_message(lake) -> str:
    """`table()` never reaches DuckDB with a bad name.

    Worth pinning because it is the counter-example: the typed accessors were already good
    at this, which is part of why the raw-SQL path's silence went unnoticed.
    """
    with pytest.raises(KeyError) as exc:
        lake.table("nosuchtable")
    return str(exc.value)


def test_the_accessor_names_the_table_asked_for(unknown_table_message):
    assert "nosuchtable" in unknown_table_message


def test_the_accessor_lists_what_is_available(unknown_table_message):
    assert "all_files" in unknown_table_message
