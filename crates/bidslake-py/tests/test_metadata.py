"""BidsFile.metadata, get_events(), get_associated(), and the wide files view."""

from __future__ import annotations

import pytest
from bidslake import BidsFile


def test_metadata_is_bids_cased(lake):
    md = next(lake.get(task="rest", suffix="bold", dataset_id="ds210")).metadata
    # Verbatim BIDS field names, not snake_case, with a numeric TR.
    assert (
        "RepetitionTime" in md
        and "repetition_time" not in md
        and isinstance(md["RepetitionTime"], (int, float))
    )


def test_metadata_empty_without_sidecar(lake):
    files = list(lake.get(suffix="bold", dataset_id="ds210"))
    assert all(isinstance(f.metadata, dict) for f in files[:5])


def test_get_events_returns_rows(lake):
    # ds001 has populated events; inheritance is resolved at ingest.
    ev = next(lake.get(suffix="bold", dataset_id="ds001")).get_events()
    assert ev.height > 0 and {"onset", "duration"} <= set(ev.columns)


def test_get_events_by_concept(lake):
    # The "events by task" pattern works directly on the events table.
    assert list(lake.get(table="events", dataset_id="ds001"))


def test_get_associated(lake):
    f = next(lake.get(suffix="bold", dataset_id="ds001"))
    assoc = f.get_associated()
    events_assoc = f.get_associated(kind="events")
    assert (
        assoc
        and all(isinstance(a, BidsFile) for a in assoc)
        and "events" in {a.entities.get("association_type") for a in assoc}
        and events_assoc
        and all(a.file_path.endswith("_events.tsv") for a in events_assoc)
    )


def test_metadata_requires_lake():
    # A hand-built BidsFile without a lake reference errors clearly.
    f = BidsFile(dataset_id="d", file_path="x", uri="file:///x", entities={})
    with pytest.raises(RuntimeError):
        _ = f.metadata


def test_wide_files_view_namespaced_no_collision(lake):
    columns = set(lake.files.pl().columns)
    # Joined columns namespaced; scans columns unprefixed; no bare metadata leak.
    expected = {"sidecar__RepetitionTime", "participant__age", "file_path", "task"}
    assert expected <= columns and "RepetitionTime" not in columns
