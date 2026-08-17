"""Opening a database and inspecting its structure."""

from __future__ import annotations

import shutil
import subprocess

import bidslake
import pytest
from bidslake.paths import to_local_path

#: The concepts live once, on the registry view; the satellites key on `file_id`.
CONCEPTS = {"sub", "ses", "task", "run", "datatype", "suffix", "extension", "modality"}


def test_tables_present(lake):
    expected = {
        "file_registry",
        "all_files",
        "dataset_roots",
        "scans",
        "sidecars",
        "participants",
        "sessions",
        "events",
        "dataset_description",
    }

    assert expected <= set(lake.tables())


def test_the_registry_carries_the_concepts(lake):
    assert set(lake.columns("all_files")) >= CONCEPTS


@pytest.mark.parametrize("satellite", ["scans", "sidecars"])
def test_a_satellite_carries_no_concepts(lake, satellite):
    assert set(lake.columns(satellite)).isdisjoint(CONCEPTS)


def test_a_satellite_is_queried_by_concept_anyway(lake):
    """A concept filter reaches a table that stores no concepts at all (docs/adr/0006).

    `get` joins a file-keyed table back to the registry, which is what makes that possible.
    `sidecars` rather than `scans`: the fixtures ship no `scans.tsv`, and `scans` is that
    file's satellite. That it stores no concepts is pinned above.
    """
    hits = list(lake.get(table="sidecars", suffix="bold"))

    assert hits


def test_unknown_table_raises(lake):
    with pytest.raises(KeyError):
        lake.table("no_such_table")


# -- the ingest roots, and what may be concluded from them -------------------


@pytest.fixture(scope="module")
def roots(lake):
    df = lake.roots()
    assert df.height > 0, "fixture assumption: the catalog records the roots it ingested"
    return df


def test_roots_reports_location_and_tenure(roots):
    assert set(roots.columns) == {"dataset_id", "root_uri", "tenure"}


def test_an_ordinary_index_records_attached_tenure(roots):
    """`attached` is what an ordinary `index` promises.

    `tenure` is what stops a consumer treating a catalog row as a statement about the
    present (docs/adr/0007).
    """
    assert set(roots["tenure"].to_list()) == {"attached"}


def test_roots_can_be_scoped_to_one_dataset(lake, roots):
    """A query that means one tree has to be able to name it."""
    one = roots["dataset_id"][0]

    scoped = lake.roots(one)

    assert set(scoped["dataset_id"].to_list()) == {one}


def test_to_uri_matches_what_the_catalog_stored(roots):
    """`to_uri` is the write direction of the stored `root_uri`.

    A consumer scoping a query to an output tree writes `root_uri = to_uri(dst)`, so the two
    spellings have to agree exactly — including symlink resolution, which is where `/tmp`
    versus `/private/tmp` would otherwise silently match nothing.
    """
    stored = roots["root_uri"].to_list()

    round_tripped = {uri: bidslake.to_uri(to_local_path(uri)) for uri in stored}

    assert round_tripped == {uri: uri for uri in stored}


# -- reading a catalog older than the `tenure` column -------------------------


@pytest.fixture(scope="module")
def pre_tenure_roots(tmp_path_factory: pytest.TempPathFactory):
    """`roots()` over a catalog whose `dataset_roots` predates the `tenure` column.

    Fabricated with the `duckdb` CLI, because the old shape predates anything this package
    can write: `bidslake.open` is read-only, and `index` would build the current schema.
    """
    cli = shutil.which("duckdb")
    if cli is None:
        pytest.skip("no duckdb CLI to fabricate a pre-tenure catalog with")

    db = tmp_path_factory.mktemp("pre-tenure") / "old.duckdb"
    subprocess.run(
        [cli, str(db)],
        input=(
            "CREATE TABLE dataset_roots (dataset_id TEXT, root_uri TEXT, "
            "PRIMARY KEY (dataset_id, root_uri));"
            "INSERT INTO dataset_roots VALUES ('study', 'file:///data/study');"
        ),
        text=True,
        check=True,
        capture_output=True,
    )
    return bidslake.open(str(db)).roots()


def test_a_pre_tenure_catalog_still_opens(pre_tenure_roots):
    """A reader that insisted on the column could not open such a catalog at all.

    `index` is the only command that runs DDL, and this package opens read-only.
    """
    assert set(pre_tenure_roots.columns) == {"dataset_id", "root_uri", "tenure"}


def test_a_pre_tenure_root_reads_as_attached(pre_tenure_roots):
    """`attached` because it is what that root actually promised.

    The same value the write-side backfill (`ALTER TABLE … DEFAULT 'attached'`) would have
    given it.
    """
    assert pre_tenure_roots["tenure"].to_list() == ["attached"]
