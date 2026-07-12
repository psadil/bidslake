"""Invalid usages that MUST produce type errors (asserted by test_typing)."""

from __future__ import annotations

import bidslake


def use(lake: bidslake.BidsLake) -> None:
    lake.get(tsk="rest")  # unknown filter key
    lake.get(datatype="fnuc")  # not a valid Datatype
    lake.get(suffix="notarealsuffix")  # not a valid Suffix
