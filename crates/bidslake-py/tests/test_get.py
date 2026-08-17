"""The headline typed iterator: get() by BIDS concept."""

from __future__ import annotations

import pytest
from bidslake import BidsFile


@pytest.fixture(scope="module")
def rest_bold(lake):
    """Every task-rest BOLD image across the fixture catalog.

    Pinned to the extension because `get()` yields sidecars too, and the first task-rest
    bold is ds210's inherited `task-rest_echo-3_bold.json`, which has no `sub` and is not
    an image.
    """
    files = list(lake.get(task="rest", suffix="bold", extension=".nii.gz"))
    assert files, "fixture assumption: the fixture datasets ship task-rest BOLD"
    return files


def test_get_yields_bids_files(rest_bold):
    assert all(isinstance(f, BidsFile) for f in rest_bold)


def test_every_hit_matches_the_task_filter(rest_bold):
    assert {f.task for f in rest_bold} == {"rest"}


def test_every_hit_matches_the_suffix_filter(rest_bold):
    assert {f.suffix for f in rest_bold} == {"bold"}


def test_every_hit_resolves_to_a_file_on_disk(rest_bold):
    missing = [f.file_path for f in rest_bold if not f.local_path.exists()]

    assert missing == []


def test_iterator_spans_datasets(lake):
    # ds210 + eyetracking_fmri have task-rest bold; ds001 does not.
    datasets = {f.dataset_id for f in lake.get(task="rest", suffix="bold")}

    assert datasets == {"ds210", "eyetracking_fmri"}


def test_ses_none_selects_sessionless(lake):
    # ds210 has no sessions; eyetracking_fmri does — ses IS NULL isolates ds210.
    files = list(lake.get(task="rest", suffix="bold", ses=None))

    assert {f.dataset_id for f in files} == {"ds210"}


def test_sequence_filter_in_clause(lake):
    """Every named subject comes back, and nothing else.

    A subset would also pass a containment check while meaning the `IN` had dropped one.
    """
    files = list(lake.get(suffix="bold", sub=["01", "02"]))

    assert {f.sub for f in files} == {"01", "02"}


def test_empty_sequence_matches_nothing(lake):
    assert list(lake.get(suffix=[])) == []


def test_unknown_column_raises(lake):
    with pytest.raises(KeyError):
        list(lake.get(nonsense="x"))


# -- the entities mapping on a single hit ------------------------------------


@pytest.fixture(scope="module")
def one_rest_bold(rest_bold):
    return rest_bold[0]


def test_entities_carry_the_queried_concepts(one_rest_bold):
    queried = {k: one_rest_bold.entities.get(k) for k in ("task", "suffix")}

    assert queried == {"task": "rest", "suffix": "bold"}


def test_entities_carry_the_subject(one_rest_bold):
    assert one_rest_bold.sub is not None


def test_the_hit_is_the_image_not_its_sidecar(one_rest_bold):
    assert one_rest_bold.local_path.name.endswith("_bold.nii.gz")
