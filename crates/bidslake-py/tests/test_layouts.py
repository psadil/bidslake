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


# ---------------------------------------------------------------------------
# Placeholders bound once, at `under()`
#
# `feat`'s two `classification` roles render `fix4melview_{training}_thr{threshold}.txt`,
# and those values are the pipeline's configuration rather than anything the layout knows.
# Raising for an unbound one is right (docs/adr/0008) and is pinned above. What these add is
# that the values may be supplied *once*, where the root is, since they have the same
# lifetime — before this, `for role in layout.roles: at[role]` was an error and every
# consumer that walked the role list special-cased the same two names by hand.
# ---------------------------------------------------------------------------

BINDINGS = {"training": "UKBiobank", "threshold": "1", "rater": "psadil"}


def test_bindings_given_to_under_are_inherited(feat: bidslake.Layout) -> None:
    at = feat.under(Path("/work") / UNIT, training="UKBiobank", threshold="1")
    assert at["classification"] == Path(f"/work/{UNIT}/fix4melview_UKBiobank_thr1.txt")


def test_a_per_call_keyword_overrides_a_bound_one(feat: bidslake.Layout) -> None:
    at = feat.under(Path("/work") / UNIT, **BINDINGS)
    assert at.path("classification", threshold="20") == Path(
        f"/work/{UNIT}/fix4melview_UKBiobank_thr20.txt"
    )
    # …and the binding is not mutated by having been overridden once.
    assert at["classification"] == Path(f"/work/{UNIT}/fix4melview_UKBiobank_thr1.txt")


def test_a_per_call_keyword_completes_a_partial_binding(feat: bidslake.Layout) -> None:
    """The `rater` case: run-wide values bound once, the per-file one supplied late."""
    at = feat.under(Path("/work") / UNIT, training="UKBiobank", threshold="1")
    assert at.path("classification_by_rater", rater="ab") == Path(
        f"/work/{UNIT}/fix4melview_UKBiobank_thr1_ab.txt"
    )


def test_every_role_renders_when_the_placeholders_are_bound(feat: bidslake.Layout) -> None:
    """The point of the change: walking `roles` needs no knowledge of which are special."""
    at = feat.under(Path("/work") / UNIT, **BINDINGS)
    rendered = [at[role] for role in feat.roles]
    assert len(rendered) == len(feat.roles)
    assert all(p.is_absolute() for p in rendered)


def test_binding_nothing_still_raises(feat: bidslake.Layout) -> None:
    """The guarantee ADR 0008 states is unchanged; only the moment of binding moved."""
    at = feat.under(Path("/work") / UNIT)
    with pytest.raises(KeyError, match="unbound placeholders"):
        at["classification"]


def test_a_mistyped_binding_says_what_is_bound(feat: bidslake.Layout) -> None:
    """Otherwise `under(root, trainng=…)` fails as 'unbound' to a caller who did bind it."""
    at = feat.under(Path("/work") / UNIT, trainng="UKBiobank", threshold="1")
    with pytest.raises(KeyError, match=r"bound here: threshold='1', trainng='UKBiobank'"):
        at["classification"]


def test_mkdir_inherits_the_bindings(feat: bidslake.Layout, tmp_path: Path) -> None:
    at = feat.under(tmp_path / UNIT, **BINDINGS)
    target = at.mkdir("classification")
    assert target.parent.is_dir()
    assert target.name == "fix4melview_UKBiobank_thr1.txt"


def test_under_without_bindings_is_unchanged(feat: bidslake.Layout) -> None:
    """The 21 roles that need nothing keep working exactly as before."""
    at = feat.under(Path("/work") / UNIT)
    assert at["filtered_func"] == Path(f"/work/{UNIT}/filtered_func_data.nii.gz")
    assert at["highres2standard_mat"] == Path(f"/work/{UNIT}/reg/highres2standard.mat")
