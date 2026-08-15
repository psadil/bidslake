"""Opening a database and inspecting its structure."""

from __future__ import annotations

import pytest


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


def test_registry_has_concept_columns(lake):
    # The concepts live once, on the registry view; the satellites key on `file_id`.
    concepts = {"sub", "ses", "task", "run", "datatype", "suffix", "extension", "modality"}
    assert concepts <= set(lake.columns("all_files"))
    assert set(lake.columns("scans")).isdisjoint(concepts)


def test_a_satellite_is_queried_by_concept_anyway(lake):
    # `get` joins a file-keyed table back to the registry, so a concept filter reaches
    # a table that stores no concepts at all (docs/adr/0006). `sidecars` rather than
    # `scans`: the fixtures ship no `scans.tsv`, and `scans` is that file's satellite.
    assert not {"suffix", "task"} & set(lake.columns("sidecars"))
    assert list(lake.get(table="sidecars", suffix="bold"))


def test_unknown_table_raises(lake):
    with pytest.raises(KeyError):
        lake.table("no_such_table")


def test_raw_sql_escape_hatch(lake):
    df = lake.sql(
        "SELECT count(*) AS n FROM all_files WHERE datatype = ? AND suffix = ?",
        ["func", "bold"],
    )
    assert df["n"][0] > 0


def test_roots_reports_tenure(lake):
    """Every root carries a tenure, and `attached` is what an ordinary index records.

    The column is what stops a consumer treating a catalog row as a statement about the
    present (docs/adr/0009); `roots()` is how it becomes visible from Python at all.
    """
    df = lake.roots()
    assert df.height > 0
    assert set(df.columns) == {"dataset_id", "root_uri", "tenure"}
    assert set(df["tenure"].to_list()) == {"attached"}
    # Scoped to one dataset, since a query that means one tree has to name it.
    one = df["dataset_id"][0]
    assert lake.roots(one)["dataset_id"].to_list() == [one] * lake.roots(one).height


def test_to_uri_matches_what_the_catalog_stored(lake):
    """`to_uri` is the write direction of the stored `root_uri`.

    A consumer scoping a query to an output tree writes `root_uri = to_uri(dst)`, so the two
    spellings have to agree exactly — including symlink resolution, which is where `/tmp`
    versus `/private/tmp` would otherwise silently match nothing.
    """
    import bidslake
    from bidslake.paths import to_local_path

    for uri in lake.roots()["root_uri"].to_list():
        assert bidslake.to_uri(to_local_path(uri)) == uri


def test_roots_reads_a_catalog_written_before_tenure(tmp_path):
    """An older catalog has no `tenure` column, and reading must not require one.

    `index` is the only command that runs DDL, and this package opens read-only, so a reader
    that insisted on the column could not open such a catalog at all. `attached` is reported
    because it is what that root actually promised — the same value the write-side backfill
    (`ALTER TABLE ... DEFAULT 'attached'`) would have given it.
    """
    import shutil
    import subprocess

    import bidslake

    # Fabricated with the CLI, because the old shape predates anything this package can
    # write: `bidslake.open` is read-only, and `index` would build the current schema.
    cli = shutil.which("duckdb")
    if cli is None:
        pytest.skip("no duckdb CLI to fabricate a pre-tenure catalog with")

    db = tmp_path / "old.duckdb"
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

    df = bidslake.open(str(db)).roots()
    assert df["tenure"].to_list() == ["attached"]
    assert set(df.columns) == {"dataset_id", "root_uri", "tenure"}
