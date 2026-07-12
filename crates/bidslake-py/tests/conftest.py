"""Test fixtures.

The end-to-end fixture ingests two bids-examples datasets with the Rust `index`
command into a temporary DuckDB, then opens it through the Python package:

- ``ds210`` — sessionless, has task-rest BOLD.
- ``eyetracking_fmri`` — has sessions, also task-rest BOLD.
- ``ds001`` — sessionless, has populated ``_events.tsv`` (for get_events).

Together they exercise the cross-dataset pool, the ``ses IS NULL`` path, and
real event rows.

Set ``BIDSLAKE_TEST_DB`` to an already-built database to skip the (slow) ingest
while iterating locally.
"""

from __future__ import annotations

import os
import subprocess
from pathlib import Path

import pytest

DATASETS = ["ds210", "eyetracking_fmri", "ds001"]


def _repo_root() -> Path:
    out = subprocess.check_output(["git", "rev-parse", "--show-toplevel"], text=True)
    return Path(out.strip())


@pytest.fixture(scope="session")
def lake_db(tmp_path_factory: pytest.TempPathFactory) -> Path:
    override = os.environ.get("BIDSLAKE_TEST_DB")
    if override:
        return Path(override)

    repo = _repo_root()
    examples = repo / "crates" / "bidslake" / "tests" / "bids-examples"
    if not (examples / DATASETS[0]).exists():
        pytest.skip("bids-examples submodule not initialized")

    db = tmp_path_factory.mktemp("bidslake") / "test.duckdb"
    for name in DATASETS:
        subprocess.run(
            [
                "cargo",
                "run",
                "-q",
                "--release",
                "-p",
                "bidslake",
                "--",
                "index",
                "--input",
                str(examples / name),
                "--output",
                str(db),
                "--dataset-id",
                name,
            ],
            cwd=repo,
            check=True,
        )
    return db


@pytest.fixture(scope="session")
def lake(lake_db: Path):
    import bidslake

    return bidslake.open(str(lake_db))
