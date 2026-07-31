"""Layouts: building output paths for files that do not exist yet.

The read side (term maps) and the write side (layouts) describe one tree from opposite
directions. These pin the write side's behaviour; that the two *agree* is enforced when
the layout loads, and exercised here by round-tripping a rendered path back through an
actual index of a tree built from those very paths.
"""

from __future__ import annotations

import subprocess
from pathlib import Path

import bidslake
import pytest

UNIT = "sub-01_ses-V1_task-rest_run-01_desc-preproc_bold"


@pytest.fixture(scope="module")
def feat() -> bidslake.Layout:
    return bidslake.layout("feat")


def test_roles_cover_the_feat_tree(feat: bidslake.Layout) -> None:
    # The slots a FIX/MELODIC run actually produces and downstream code reaches for.
    for role in (
        "filtered_func",
        "filtered_func_clean",
        "mask",
        "melodic_mix",
        "motion_params",
        "classification",
        "example_func",
        "highres",
        "standard",
        "wmparc",
        "highres2standard_mat",
        "standard2example_func_warp",
    ):
        assert role in feat.roles
    assert feat.term_map == "feat"


def test_literal_roles_render_under_a_root(feat: bidslake.Layout) -> None:
    at = feat.under(Path("/work") / UNIT)
    assert at["filtered_func_clean"] == Path(f"/work/{UNIT}/filtered_func_data_clean.nii.gz")
    assert at["highres2standard_mat"] == Path(f"/work/{UNIT}/reg/highres2standard.mat")
    assert at["melodic_mix"] == Path(f"/work/{UNIT}/filtered_func_data.ica/melodic_mix")


def test_placeholders_come_from_keywords(feat: bidslake.Layout) -> None:
    at = feat.under(Path("/work") / UNIT)
    assert at.path("classification", training="UKBiobank", threshold="1") == Path(
        f"/work/{UNIT}/fix4melview_UKBiobank_thr1.txt"
    )
    assert at.path(
        "classification_by_rater", training="UKBiobank", threshold="1", rater="psadil"
    ) == Path(f"/work/{UNIT}/fix4melview_UKBiobank_thr1_psadil.txt")


def test_unbound_placeholder_raises_rather_than_guessing(feat: bidslake.Layout) -> None:
    """An empty substitution would produce a plausible path pointing at nothing."""
    at = feat.under(Path("/work") / UNIT)
    with pytest.raises(KeyError, match="unbound placeholders"):
        at["classification"]


def test_unknown_role_lists_what_exists(feat: bidslake.Layout) -> None:
    at = feat.under(Path("/work") / UNIT)
    with pytest.raises(KeyError, match="unknown role"):
        at["highres2standard"]  # the real role name ends in `_mat`


def test_unknown_layout_names_the_bundled_ones() -> None:
    with pytest.raises(RuntimeError, match="bundled layouts"):
        bidslake.layout("nope")


def test_mkdir_creates_the_parent(feat: bidslake.Layout, tmp_path: Path) -> None:
    target = feat.under(tmp_path / UNIT).mkdir("highres2standard_mat")
    assert target.parent.is_dir()
    assert not target.exists(), "only the directory is created, not the file"


def test_rendered_paths_index_back_as_declared(feat: bidslake.Layout, tmp_path: Path) -> None:
    """The end-to-end version of the round trip the layout checks at load time.

    Build a tree *from the layout's own rendered paths*, index it with the `feat`
    adapter, and confirm each file reads back as the role declared. This is what makes
    the two directions trustworthy rather than merely coexisting.
    """
    binary = (
        Path(subprocess.check_output(["git", "rev-parse", "--show-toplevel"], text=True).strip())
        / "target"
        / "debug"
        / "bidslake"
    )
    if not binary.exists():
        pytest.skip("build the debug binary first: cargo build -p bidslake")

    root = tmp_path / "out"
    at = feat.under(root / UNIT)
    # `desc` is worth reading carefully. Where a role's mapping sets it, the projection
    # wins. Where it does not (`highres`, `wmparc` -- identified by `suffix` and `seg`
    # instead), the concept column falls back to the path regex, which finds
    # `desc-preproc` in the *unit directory* name. That is the input BOLD's desc, and
    # inheriting it is a true statement about which run the tree was built from.
    expected = {
        "filtered_func_clean": ("func", "bold", "clean"),
        "mask": ("func", "mask", "brain"),
        "melodic_mix": ("func", "mixing", "MELODIC"),
        "highres": ("anat", "T1w", "preproc"),
        "wmparc": ("anat", "dseg", "preproc"),
    }
    for role in expected:
        at.mkdir(role).write_bytes(b"x")

    db = tmp_path / "out.duckdb"
    subprocess.run(
        [
            str(binary),
            "index",
            "-i",
            str(root),
            "-o",
            str(db),
            "--dataset-id",
            "out",
            "--adapter",
            "feat",
        ],
        check=True,
        capture_output=True,
    )

    with bidslake.open(str(db)) as lake:
        rows = {
            Path(f.file_path).name: (f.datatype, f.suffix, f.entities.get("desc"))
            for f in lake.get()
        }
    for role, want in expected.items():
        name = at[role].name
        assert name in rows, f"{role} ({name}) was not indexed"
        assert rows[name] == want, f"{role} read back as {rows[name]}, declared {want}"
