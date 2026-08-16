"""Schema-augmentation (overlay) behavior on the Python side.

Builds a tiny fMRIPrep-style derivative database with the bundled `fmriprep`
overlay, then checks that augmented columns are queryable at runtime with no extra
step, that the overlay provenance and effective schema are recoverable, and that the
opt-in stubgen types the augmented schema.

This file is also the only place the model layer's keyword-column rule is exercised: a
column named `from` cannot be a Python attribute and is emitted as `from_`. The bundled
BIDS schema declares no such column, so it can only be pinned against a catalog whose
overlay adds one — see `GOOD_QUERY` below.
"""

from __future__ import annotations

import subprocess
import sys
from collections.abc import Callable
from pathlib import Path

import bidslake
import pytest
from bidslake import stubgen


@pytest.fixture(scope="module")
def augmented_db(
    tmp_path_factory: pytest.TempPathFactory, bidslake_cli: Callable[..., None]
) -> str:
    root = tmp_path_factory.mktemp("deriv")
    (root / "sub-01" / "func").mkdir(parents=True)
    (root / "dataset_description.json").write_text(
        '{"Name":"deriv","BIDSVersion":"1.11.1","DatasetType":"derivative"}'
    )
    func = root / "sub-01" / "func"
    (func / "sub-01_task-rest_desc-preproc_bold.nii.gz").write_bytes(b"")
    (func / "sub-01_task-rest_desc-preproc_bold.json").write_text('{"RepetitionTime":2.0}')
    (func / "sub-01_task-rest_desc-confounds_timeseries.tsv").write_text(
        "trans_x\ttrans_y\n0.10\t0.20\n0.11\t0.21\n0.12\t0.22\n"
    )

    db = tmp_path_factory.mktemp("db") / "aug.duckdb"
    bidslake_cli("index", "-i", root, "-o", db, "--adapter", "fmriprep")
    return str(db)


@pytest.fixture(scope="module")
def aug_lake(augmented_db: str):
    with bidslake.open(augmented_db) as lake:
        yield lake


def test_the_overlay_is_recorded_as_provenance(aug_lake) -> None:
    assert [source for _idx, source, _sha in aug_lake.overlays] == ["fmriprep"]


def test_the_effective_schema_carries_the_overlays_rules(aug_lake) -> None:
    """What the catalog was actually built against, recoverable after the fact."""
    schema = aug_lake.effective_schema()

    assert schema is not None and "fmriprep" in schema["rules"]["tabular_data"]


# -- the augmented vocabulary at runtime, with no codegen step ---------------


def test_the_preprocessed_image_is_found_by_its_base_entities(aug_lake) -> None:
    # `kind` because `get` iterates the whole registry: the image and its sidecar share
    # every entity, and which of them you want is the caller's to say.
    files = list(aug_lake.get(desc="preproc", suffix="bold", kind="data"))

    assert len(files) == 1


def test_without_kind_the_sidecar_is_a_row_too(aug_lake) -> None:
    files = list(aug_lake.get(desc="preproc", suffix="bold"))

    assert len(files) == 2


def test_the_overlays_table_exists_with_its_columns(aug_lake) -> None:
    assert "trans_x" in aug_lake.columns("fmriprep_confounds")


@pytest.fixture(scope="module")
def confounds(aug_lake):
    return aug_lake.sql("SELECT row_idx, trans_x FROM fmriprep_confounds ORDER BY row_idx")


def test_the_overlays_rows_keep_their_order(confounds) -> None:
    assert confounds["row_idx"].to_list() == [0, 1, 2]


def test_the_overlays_values_are_typed(confounds) -> None:
    assert confounds["trans_x"].to_list() == [0.10, 0.11, 0.12]


# -- re-emitting the types from an augmented catalog -------------------------


@pytest.fixture(scope="module")
def stub_module(augmented_db: str) -> str:
    return stubgen.generate(augmented_db)


@pytest.mark.parametrize(
    ("fragment", "why"),
    [
        ('"timeseries"', "augmented Suffix should include timeseries"),
        ("class fmriprep_confounds", "C should gain the augmented table"),
        ('"from"', "augmented entity should reach GetFilters/Entity"),
    ],
)
def test_stubgen_types_the_augmented_schema(stub_module: str, fragment: str, why: str) -> None:
    assert fragment in stub_module, why


GOOD_QUERY = '''\
"""Queries over overlay-added vocabulary: must type-check."""

from sqlalchemy import select

from _bids_types import AllFiles, FmriprepConfounds, GetFilters

# `from`/`to` are fMRIPrep's entities, not BIDS'; against the bundled `GetFilters`
# neither key exists.
preproc: GetFilters = {"datatype": "func", "suffix": "bold", "desc": "preproc"}
xfm: GetFilters = {"suffix": "xfm", "from": "boldref", "to": "T1w"}

# The models the same catalog generated. `from` is a Python keyword, so its column
# is reachable as `from_`; every other entity keeps its own name.
Q = (
    select(AllFiles.file_path, AllFiles.to, FmriprepConfounds.trans_x)
    .join(FmriprepConfounds, FmriprepConfounds.file_id == AllFiles.file_id)
    .where(AllFiles.from_ == "boldref")
)
'''

BAD_QUERY = '''\
"""Queries the augmented vocabulary must still reject."""

from sqlalchemy import select

from _bids_types import AllFiles, FmriprepConfounds, GetFilters

bad_key: GetFilters = {"frm": "boldref"}
bad_suffix: GetFilters = {"suffix": "notarealsuffix"}
bad_datatype: GetFilters = {"datatype": "fnuc"}
bad_column = select(AllFiles.notanentity)
bad_overlay_column = select(FmriprepConfounds.notacolumn)
'''

#: One per deliberately-wrong name in `BAD_QUERY`. Asserted individually so a check that
#: stops firing names itself rather than hiding behind the four that still do.
REJECTED = ["frm", "notarealsuffix", "fnuc", "notanentity", "notacolumn"]


def _ty(path: Path, search: Path) -> subprocess.CompletedProcess[str]:
    ty = Path(sys.executable).with_name("ty")
    if not ty.exists():
        pytest.skip("ty not installed in this environment")
    # `sys.prefix`, not `Path(sys.executable).resolve()`: resolving follows the venv's
    # symlink back to the base interpreter, whose site-packages has none of the
    # third-party imports the fixtures make (`sqlalchemy`), so every one of them would
    # be reported unresolved and the "good" fixture would fail for the wrong reason.
    return subprocess.run(
        [str(ty), "check", "--python", sys.prefix, "--extra-search-path", str(search), str(path)],
        capture_output=True,
        text=True,
        cwd=Path(__file__).resolve().parents[1],
    )


@pytest.fixture(scope="module")
def typed_workspace(stub_module: str, tmp_path_factory: pytest.TempPathFactory) -> Path:
    """The generated types beside the two query fixtures that check against them."""
    workspace = tmp_path_factory.mktemp("stubs")
    (workspace / "_bids_types.py").write_text(stub_module)
    (workspace / "good_query.py").write_text(GOOD_QUERY)
    (workspace / "bad_query.py").write_text(BAD_QUERY)
    return workspace


@pytest.fixture(scope="module")
def good_query_result(typed_workspace: Path) -> subprocess.CompletedProcess[str]:
    return _ty(typed_workspace / "good_query.py", typed_workspace)


@pytest.fixture(scope="module")
def bad_query_output(typed_workspace: Path) -> tuple[int, str]:
    result = _ty(typed_workspace / "bad_query.py", typed_workspace)
    return result.returncode, result.stdout + result.stderr


def test_overlay_vocabulary_type_checks(good_query_result) -> None:
    """The bundled `GetFilters` and models are pinned to the BIDS schema this build ships,
    so `from`/`to`/`trans_x` are type errors there; the point of re-emitting them from a
    catalog is that they stop being errors — including `AllFiles.from_`, the keyword column.
    """
    assert good_query_result.returncode == 0, good_query_result.stdout + good_query_result.stderr


def test_bad_queries_are_still_rejected(bad_query_output) -> None:
    """...*without* the vocabulary going untyped: a real name is not made up for."""
    returncode, _ = bad_query_output

    assert returncode != 0


@pytest.mark.parametrize("name", REJECTED)
def test_each_invented_name_is_flagged(bad_query_output, name: str) -> None:
    _, output = bad_query_output

    assert name in output, f"{name} was not flagged:\n{output}"
