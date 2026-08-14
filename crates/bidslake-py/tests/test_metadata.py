"""BidsFile.metadata, get_events(), get_associated(), and the wide files view."""

from __future__ import annotations

import pytest
from bidslake import BidsFile


def test_metadata_is_bids_cased(lake):
    # `kind="data"`, because `get()` yields sidecars too and the query has no ORDER BY:
    # ds210's inherited `task-rest_echo-3_bold.json` can come back first, and a sidecar
    # carries no metadata of its own, so `md` is then `{}` and there is nothing to check.
    md = next(lake.get(task="rest", suffix="bold", dataset_id="ds210", kind="data")).metadata
    # Verbatim BIDS field names, not snake_case, with a numeric TR.
    assert (
        "RepetitionTime" in md
        and "repetition_time" not in md
        and isinstance(md["RepetitionTime"], (int, float))
    )


def test_metadata_excludes_the_join_key(lake):
    # `sidecars` keys on `file_id` now (docs/adr/0006). The key is plumbing, not a BIDS
    # metadata field, and it leaked into every `metadata` dict as a `Decimal` when this
    # skipped the `(dataset_id, file_path)` pair it replaced.
    # `kind="data"` for the same reason as above, and here it is what gives the assertion
    # teeth: against a sidecar's empty `metadata` the intersection is trivially empty.
    md = next(lake.get(task="rest", suffix="bold", dataset_id="ds210", kind="data")).metadata
    assert md, "an empty dict would satisfy the assertion below without testing anything"
    assert not {"file_id", "dataset_id", "file_path", "other_data"} & set(md)


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


def test_wide_files_view_namespaced_no_collision(lake):
    columns = set(lake.files.pl().columns)
    # Joined columns namespaced; scans columns unprefixed; no bare metadata leak.
    expected = {"sidecar__RepetitionTime", "participant__age", "file_path", "task"}
    assert expected <= columns and "RepetitionTime" not in columns


def test_get_described_by_reads_the_same_rows_as_get_events(lake):
    """`get_events()` is one instance of a general relation, not a special case.

    Both go through `file_associations`: the rows live once against the file that
    *describes* the run, and the edge says which runs it describes (docs/adr/0007). The
    generic accessor takes the association name, which is also the table name.
    """
    bold = next(lake.get(suffix="bold", dataset_id="ds001"))
    assert bold.get_described_by("events", order_by="onset").equals(bold.get_events())


def test_get_described_by_is_empty_for_an_absent_table(lake):
    """An adapter's table is missing from a catalog built without that adapter. That is an
    answer — "this catalog holds no such rows" — not an error, and the same shape a run with
    no describing file gives back.
    """
    bold = next(lake.get(suffix="bold", dataset_id="ds001"))
    assert bold.get_described_by("fmriprep_confounds").is_empty()
