"""What a Python value becomes when it is bound as a SQL parameter.

``py_to_duck_value`` tries the Python scalar types in order and takes the first that fits.
An ``int`` too large for ``i64`` therefore falls past the ``BigInt`` arm into the ``Double``
one, where it is not rejected — it is rounded to the nearest ``f64`` and comes back a
different number than the one that went in.

That matters because ``file_id`` is a ``UBIGINT`` derived from a SHA-256, so half of every
catalog's ids are above ``i64::MAX``. ``WHERE file_id = ?`` returned no rows for them.

Property-based rather than a case table for a reason worth keeping: the boundary anyone
would write by hand, ``2**63``, *passes*. It is exactly representable as an ``f64``, so the
round trip happens to be lossless there. The first value that fails is ``2**63 + 1``,
because ``f64`` spacing at that magnitude is 2048.

This is the other half of a trip docs/adr/0006 already documents. ``file_id`` moved from
``HUGEINT`` to ``UBIGINT`` because ``HUGEINT`` "does not survive the trip to Python" — the
Arrow bridge handed it over as ``Decimal128(38, 0)`` and 41% of the id space fell outside
its own declared type. That fixed the *result* direction. This is the *parameter*
direction, where ``i64`` was exactly as narrow for a ``UBIGINT`` as ``Decimal128`` was for a
``HUGEINT``.
"""

from __future__ import annotations

from hypothesis import example, given
from hypothesis import strategies as st

#: DuckDB's ``UBIGINT`` is the unsigned 64-bit range, so everything in it has an exact
#: representation and everything in it must survive the round trip.
UINT64_MAX = 2**64 - 1


@given(n=st.integers(min_value=0, max_value=UINT64_MAX))
@example(n=2**63 + 1)
def test_an_integer_bind_parameter_comes_back_as_the_same_integer(lake, n):
    """The regression: ``2**63 + 1`` bound as a parameter used to come back as ``2**63``.

    The ``@example`` is what makes this able to fail at ``max_examples=1``; the generated
    cases around it are there to catch the next place the fallthrough order goes wrong, not
    to carry this one.
    """
    bound = lake.sql("SELECT ?::UBIGINT AS bound", [n])

    assert bound["bound"][0] == n


@given(n=st.integers(min_value=0, max_value=UINT64_MAX))
def test_an_integer_bind_parameter_keeps_its_type_across_the_boundary(lake, n):
    """A bound integer arrives as an integer, not as a float that happens to compare equal.

    Separate from the round trip above because it is a different failure: a value below
    ``2**53`` survives the ``f64`` arm numerically while still crossing as a ``DOUBLE``, so
    equality alone cannot see that the typed bind was skipped.
    """
    kind = lake.sql("SELECT typeof(?) AS kind", [n])

    assert kind["kind"][0] in {"BIGINT", "UBIGINT", "HUGEINT", "UHUGEINT"}
