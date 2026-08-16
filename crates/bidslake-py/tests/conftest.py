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
from collections.abc import Callable
from pathlib import Path

import pytest
from hypothesis import settings

DATASETS = ["ds210", "eyetracking_fmri", "ds001"]


@pytest.fixture(scope="session")
def repo_root() -> Path:
    out = subprocess.check_output(["git", "rev-parse", "--show-toplevel"], text=True)
    return Path(out.strip())


@pytest.fixture(scope="session")
def bidslake_cli(repo_root: Path) -> Callable[..., None]:
    """Run the `bidslake` CLI, building it once for the whole session.

    Built rather than `cargo run`-ed per call for two reasons. Every fixture here reaches
    for the *same* binary, so probing `target/debug/bidslake` and skipping when it is absent
    (which is what `test_adapter`, `test_overlay` and `test_layouts` used to do) silently
    dropped a tenth of the suite wherever nobody had run a debug build — CI included, since
    its Python job never builds one. And `cargo run` re-checks the whole workspace's
    freshness on every invocation, which this fixture alone makes seventeen times.

    `--release` because that is the profile the session ingest already needs; asking for
    debug as well would mean compiling the bundled DuckDB engine a second time.
    """
    subprocess.run(
        ["cargo", "build", "-q", "--release", "-p", "bidslake"],
        cwd=repo_root,
        check=True,
    )
    binary = repo_root / "target" / "release" / "bidslake"

    def run(*args: str | Path) -> None:
        subprocess.run([str(binary), *map(str, args)], check=True, capture_output=True)

    return run


@pytest.fixture(scope="session")
def lake_db(
    request: pytest.FixtureRequest,
    tmp_path_factory: pytest.TempPathFactory,
    repo_root: Path,
) -> Path:
    override = os.environ.get("BIDSLAKE_TEST_DB")
    if override:
        return Path(override)

    # Requested here rather than as a parameter so that the override path needs no cargo
    # at all — building the CLI is the expensive half of this fixture.
    bidslake_cli: Callable[..., None] = request.getfixturevalue("bidslake_cli")

    examples = repo_root / "crates" / "bidslake" / "tests" / "bids-examples"
    if not (examples / DATASETS[0]).exists():
        pytest.skip("bids-examples submodule not initialized")

    db = tmp_path_factory.mktemp("bidslake") / "test.duckdb"

    for name in DATASETS:
        bidslake_cli("index", "--input", examples / name, "--output", db, "--dataset-id", name)

    # Let every dataset reach every dataset by name, so a binding can scope an input with
    # `via=` instead of a hardcoded id. Declared in *each* dataset, including for itself,
    # because a link is resolved relative to the dataset the anchors live in — a name
    # declared only in a neighbour is not in scope for a binding anchored here.
    for definer in DATASETS:
        for name in DATASETS:
            bidslake_cli(
                "link",
                "alias",
                "-d",
                db,
                "--dataset",
                definer,
                "--as",
                name,
                "--target",
                f"dataset:{name}",
            )
    return db


@pytest.fixture(scope="session")
def lake(lake_db: Path):
    import bidslake

    return bidslake.open(str(lake_db))


# -- hypothesis ------------------------------------------------------------------------
#
# Imported unconditionally, so a missing hypothesis is a collection error rather than a
# suite that quietly loses its property tests. A `try: import / except: skip` here would be
# the skip guard CONTRIBUTING.md bans, one level up.

#: ``deadline=None`` in every profile. Hypothesis defaults to 200 ms *per example*, and
#: every property here goes through DuckDB — prepare, execute, Arrow IPC, Polars — which is
#: over that on a cold cache and reliably over it on a loaded CI runner. A per-example wall
#: clock is the wrong instrument for a suite that talks to a database; the job's
#: ``timeout-minutes`` is the right one.
settings.register_profile("dev", max_examples=50, deadline=None)

#: ``print_blob=True`` so a CI failure carries the ``@reproduce_failure(...)`` line needed to
#: replay it locally — the runner's ``.hypothesis/`` does not survive the job.
settings.register_profile("ci", max_examples=200, deadline=None, print_blob=True)

#: For a deliberate long run: ``pytest --hypothesis-profile=thorough``.
settings.register_profile("thorough", max_examples=5000, deadline=None, print_blob=True)

# Selected from the environment rather than from `addopts`, because `--hypothesis-profile`
# is a plugin-registered flag: putting it in `addopts` would make the whole suite refuse to
# start if hypothesis were ever absent. GitHub Actions sets CI=true. An explicit
# `--hypothesis-profile` on the command line still wins over this.
settings.load_profile("ci" if os.environ.get("CI") else "dev")
