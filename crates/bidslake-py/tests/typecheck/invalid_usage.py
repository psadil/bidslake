"""Invalid usages that must produce type errors (asserted by test_typing).

Two of the three are flagged today. The unknown-key case is not — `ty` does not reject an
extra key in an `Unpack`'d `TypedDict` — and `test_typing` carries it as a strict xfail
rather than pretending otherwise.
"""

from __future__ import annotations

import bidslake


def use(lake: bidslake.BidsLake) -> None:
    lake.get(tsk="rest")  # unknown filter key — not yet flagged, see test_typing
    lake.get(datatype="fnuc")  # not a valid Datatype
    lake.get(suffix="notarealsuffix")  # not a valid Suffix
