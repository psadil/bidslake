"""The headline typed iterator: get() by BIDS concept."""

from __future__ import annotations

import pytest
from bidslake import BidsFile


def test_rest_fmri_iterator(lake):
    files = list(lake.get(task="rest", suffix="bold", extension=".nii.gz"))
    assert files and all(
        isinstance(f, BidsFile)
        and f.task == "rest"
        and f.suffix == "bold"
        and f.local_path.exists()
        for f in files
    )


def test_iterator_spans_datasets(lake):
    # ds210 + eyetracking_fmri have task-rest bold; ds001 does not.
    datasets = {f.dataset_id for f in lake.get(task="rest", suffix="bold")}
    assert datasets == {"ds210", "eyetracking_fmri"}


def test_ses_none_selects_sessionless(lake):
    # ds210 has no sessions; eyetracking_fmri does — ses IS NULL isolates ds210.
    files = list(lake.get(task="rest", suffix="bold", ses=None))
    assert files and {f.dataset_id for f in files} == {"ds210"}


def test_sequence_filter_in_clause(lake):
    files = list(lake.get(suffix="bold", sub=["01", "02"]))
    assert files and {f.sub for f in files} <= {"01", "02"}


def test_empty_sequence_matches_nothing(lake):
    assert list(lake.get(suffix=[])) == []


def test_unknown_column_raises(lake):
    with pytest.raises(KeyError):
        list(lake.get(nonsense="x"))


def test_entities_mapping(lake):
    f = next(lake.get(task="rest", suffix="bold"))
    assert (
        f.entities["task"] == "rest"
        and f.entities["suffix"] == "bold"
        and f.sub is not None
        and f.local_path.name.endswith("_bold.nii.gz")
    )
