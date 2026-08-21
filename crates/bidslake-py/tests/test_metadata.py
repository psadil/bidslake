"""BidsFile.metadata, get_events(), get_associated(), and the wide files view."""

from __future__ import annotations

import pytest
from bidslake import BidsFile


@pytest.fixture(scope="module")
def bold_metadata(lake):
    """The sidecar metadata of a ds210 task-rest BOLD image.

    `extension=".nii.gz"`, because `get()` yields sidecars too and the query has no ORDER BY:
    ds210's inherited `task-rest_echo-3_bold.json` can come back first, and a sidecar
    carries no metadata of its own — against an empty dict every assertion below is
    trivially satisfied.
    """
    f = next(lake.get(task="rest", suffix="bold", dataset_id="ds210", extension=".nii.gz"))
    md = f.metadata
    assert md, "fixture assumption: this image has sidecar metadata"
    return md


def test_metadata_uses_verbatim_bids_field_names(bold_metadata):
    assert "RepetitionTime" in bold_metadata


def test_metadata_is_not_snake_cased(bold_metadata):
    assert "repetition_time" not in bold_metadata


def test_metadata_keeps_the_value_numeric(bold_metadata):
    assert isinstance(bold_metadata["RepetitionTime"], (int, float))


def test_metadata_excludes_the_join_key(bold_metadata):
    """`sidecars` keys on `file_id` now (docs/adr/0006).

    The key is plumbing, not a BIDS metadata field, and it leaks into every `metadata` dict
    as a `Decimal` if this skips only the `(dataset_id, file_path)` pair it replaced.
    """
    assert not {"file_id", "dataset_id", "file_path", "other_data"} & set(bold_metadata)


@pytest.fixture(scope="module")
def sidecarless_file(lake):
    """A data file the catalog holds no `sidecars` row for.

    Chosen by SQL rather than by asking `metadata` for an empty dict, which is the thing
    under test.
    """
    ids = set(
        lake.sql(
            "SELECT a.file_id FROM all_files a WHERE a.extension = '.nii.gz' "
            "AND NOT EXISTS (SELECT 1 FROM sidecars s WHERE s.file_id = a.file_id)"
        )["file_id"].to_list()
    )
    assert ids, "fixture assumption: some data file in the catalog has no sidecar"
    return next(f for f in lake.get(extension=".nii.gz") if f.file_id in ids)


def test_metadata_is_empty_without_a_sidecar(sidecarless_file):
    """An empty dict, not a raise and not a partial one — "this file has no metadata"."""
    assert sidecarless_file.metadata == {}


def test_metadata_requires_lake():
    # A hand-built BidsFile without a lake reference errors clearly.
    f = BidsFile(
        dataset_id="d",
        root_uri="file:///d",
        file_path="x",
        file_id=0,
        uri="file:///x",
        entities={},
    )

    with pytest.raises(RuntimeError):
        _ = f.metadata


# -- events, and the general relation they are one instance of ---------------


@pytest.fixture(scope="module")
def ds001_bold(lake):
    """ds001 has populated events; inheritance is resolved at ingest."""
    return next(lake.get(suffix="bold", dataset_id="ds001"))


@pytest.fixture(scope="module")
def ds001_events(ds001_bold):
    return ds001_bold.get_events()


def test_get_events_returns_rows(ds001_events):
    assert ds001_events.height > 0


def test_get_events_carries_the_bids_columns(ds001_events):
    assert {"onset", "duration"} <= set(ds001_events.columns)


def test_get_events_by_concept(lake):
    # The "events by task" pattern works directly on the events table.
    assert list(lake.get(table="events", dataset_id="ds001"))


def test_get_described_by_reads_the_same_rows_as_get_events(ds001_bold, ds001_events):
    """`get_events()` is one instance of a general relation, not a special case.

    Both go through `file_associations`: the rows live once against the file that
    *describes* the run, and the edge says which runs it describes (docs/adr/0003). The
    generic accessor takes the association name, which is also the table name. That the two
    agree is the claim, so the pair of calls is one Act.
    """
    assert ds001_bold.get_described_by("events", order_by="onset").equals(ds001_events)


def test_get_described_by_is_empty_for_an_absent_table(ds001_bold):
    """An adapter's table is missing from a catalog built without that adapter.

    That is an answer — "this catalog holds no such rows" — not an error, and the same shape
    a run with no describing file gives back.
    """
    assert ds001_bold.get_described_by("fmriprep_confounds").is_empty()


# -- cross-references -------------------------------------------------------


@pytest.fixture(scope="module")
def associations(ds001_bold):
    files = ds001_bold.get_associated()
    assert files, "fixture assumption: ds001's BOLD is cross-referenced by something"
    return files


def test_associations_are_bids_files(associations):
    assert all(isinstance(a, BidsFile) for a in associations)


def test_associations_carry_their_type(associations):
    assert "events" in {a.entities.get("association_type") for a in associations}


def test_associations_can_be_filtered_by_kind(ds001_bold):
    events = ds001_bold.get_associated(association_type="events")

    # Set equality rather than `all(...)`: an empty result would satisfy `all` while
    # meaning the filter had matched nothing at all.
    assert {a.file_path.endswith("_events.tsv") for a in events} == {True}


# -- the wide joined view ----------------------------------------------------


@pytest.fixture(scope="module")
def files_view_columns(lake):
    return set(lake.files.pl().columns)


def test_joined_columns_are_namespaced(files_view_columns):
    expected = {"sidecar__RepetitionTime", "participant__age", "file_path", "task"}

    assert expected <= files_view_columns


def test_no_bare_metadata_column_leaks(files_view_columns):
    """Namespaced on purpose.

    An unprefixed `RepetitionTime` would collide with a same-named column from any other
    joined source.
    """
    assert "RepetitionTime" not in files_view_columns


def test_files_spans_every_walked_file(lake):
    """`files` is the whole registry widened, not a data-file subset.

    One row per `all_files` row — sidecars, tabular files and documentation included, with
    their joined columns simply NULL — and the caller narrows with `extension`. (It was
    once filtered to data files; this pins the widening.)
    """
    files = lake.files.pl()

    assert files.height == lake.all_files.pl().height
    assert files.filter(files["extension"] == ".json").height > 0, (
        "sidecar rows belong to the wide view too"
    )
